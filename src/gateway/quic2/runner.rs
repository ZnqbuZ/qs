use crate::gateway::quic2::conn::ConnCtrl;
use crate::gateway::quic2::endpoint::{QuicOutputTx, PACKET_POOL};
use crate::gateway::quic2::stream::{QuicStream, StreamDropRx};
use bytes::{Bytes, BytesMut};
use derive_more::{Deref, DerefMut};
use quinn_proto::{Connection, ConnectionHandle, Event, StreamEvent};
use std::collections::VecDeque;
use std::io::{Error, ErrorKind};
use std::mem::swap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::sleep;
use crate::gateway::quic2::QuicPacket;

#[derive(Debug, Deref, DerefMut)]
pub(crate) struct Runner {
    #[deref]
    #[deref_mut]
    ctrl: ConnCtrl,

    drop_rx: StreamDropRx,
    output: QuicOutputTx,
}

impl Runner {
    pub(crate) fn new(conn: Connection, output: QuicOutputTx) -> (ConnCtrl, Self) {
        let (drop_tx, drop_rx) = mpsc::channel(128);
        let ctrl = ConnCtrl::new(conn, drop_tx);
        (
            ctrl.clone(),
            Self {
                ctrl,
                drop_rx,
                output,
            },
        )
    }
}

impl Runner {
    pub(crate) async fn run(&mut self) -> std::io::Result<()> {
        let mut pending_streams = VecDeque::new();
        let mut pending_wakers = Vec::new();
        let mut inbox = Vec::new();
        let mut recv = Vec::new();
        let mut send = BytesMut::new();
        let mut pending_transmits = VecDeque::new();
        let mut pending_chunks = VecDeque::new();
        let (header, trailer) = self.output.packet.margins.into();

        let mut timer = Box::pin(sleep(Duration::MAX));
        let mut timeout;
        let mut handle_timeout = false;

        // 启动时强制唤醒一次，确保发送握手包
        self.ctrl.notify.notify_one();

        loop {
            let mut worked = false;

            // 1. --- 准备阶段：获取 Inbox ---
            swap(&mut *self.ctrl.inbox.lock(), &mut inbox);

            // 2. --- 核心逻辑：处理状态机 ---
            // [修复] 移除之前的 if !inbox.is_empty() || timeout 判断
            // 只要醒来，就必须检查状态机，因为可能需要发送握手包或者重传
            {
                let mut state = self.ctrl.state.lock();

                // 处理收到的包
                for evt in inbox.drain(..) {
                    state.conn.handle_event(evt);
                    worked = true;
                }

                // 处理超时
                if handle_timeout {
                    state.conn.handle_timeout(Instant::now());
                    handle_timeout = false;
                }

                // 处理流关闭
                loop {
                    match self.drop_rx.try_recv() {
                        Ok(id) => state.close(id),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            return Err(Error::other("Stream drop channel closed"));
                        }
                    }
                }

                // 驱动状态机 (处理握手、流开启等)
                while let Some(evt) = state.conn.poll() {
                    worked = true; // 状态机有变动，标记为工作过
                    match evt {
                        Event::Stream(StreamEvent::Opened { dir }) => {
                            if self.output.stream.switch().load(Ordering::Relaxed) {
                                while let Some(id) = state.conn.streams().accept(dir) {
                                    pending_streams.push_back(id);
                                }
                            }
                        }
                        Event::Stream(StreamEvent::Readable { id }) => {
                            if let Some(waker) = state.readers.remove(&id) {
                                pending_wakers.push(waker);
                            }
                        }
                        Event::Stream(StreamEvent::Writable { id }) => {
                            if let Some(waker) = state.writers.remove(&id) {
                                pending_wakers.push(waker);
                            }
                        }
                        Event::ConnectionLost { .. } => {
                            return Err(Error::new(ErrorKind::ConnectionReset, "Connection lost"));
                        }
                        _ => {}
                    }
                }

                // 生成待发送数据包
                let mut chunk = Vec::with_capacity(16 * 65536);
                let mut transmits = VecDeque::new();
                loop {
                    if chunk.len() + header + state.conn.current_mtu() as usize + trailer
                        > chunk.capacity()
                    {
                        pending_chunks.push_back(BytesMut::from(Bytes::from(chunk)));
                        chunk = Vec::with_capacity(16 * 65536);
                        pending_transmits.push_back(transmits);
                        transmits = VecDeque::new();
                    }
                    unsafe {
                        chunk.set_len(chunk.len() + header);
                    }
                    let Some(transmit) = state.conn.poll_transmit(Instant::now(), 1, &mut chunk)
                    else {
                        unsafe {
                            chunk.set_len(chunk.len() - header);
                        }
                        pending_chunks.push_back(BytesMut::from(Bytes::from(chunk)));
                        pending_transmits.push_back(transmits);
                        break;
                    };
                    unsafe {
                        chunk.set_len(chunk.len() + trailer);
                    }
                    transmits.push_back(transmit);
                }

                timeout = state.conn.poll_timeout();
            } // 释放 state 锁

            // 3. --- 唤醒应用层 Wakers ---
            for waker in pending_wakers.drain(..) {
                waker.wake();
            }

            // 4. --- 发送阶段：带“接收抢占”的发送 ---
            send.extend_from_slice(&recv);
            while !pending_transmits.is_empty() || !pending_streams.is_empty() {
                select! {
                    // 监听接收事件：如果有新包入队，立即停止发送，回去处理接收
                    _ = self.ctrl.notify.notified() => {
                        worked = true;
                        break;
                    }

                    // 发送 Packet
                    res = self.output.packet.reserve(), if !pending_transmits.is_empty() => {
                        match res {
                            Ok(permit) => {
                                let transmits = pending_transmits.front_mut().unwrap();
                                let chunk = pending_chunks.front_mut().unwrap();
                                let transmit = transmits.pop_front().unwrap();
                                let data = chunk.split_to(header + transmit.size + trailer);
                                if transmits.is_empty() {
                                    pending_transmits.pop_front();
                                }
                                if chunk.is_empty() {
                                    pending_chunks.pop_front();
                                }
                                let packet = QuicPacket::new(transmit.destination, data);
                                permit.send(packet);
                                worked = true;
                            }
                            Err(_) => return Err(Error::new(ErrorKind::BrokenPipe, "Packet channel closed")),
                        }
                    }

                    // 发送 Stream
                    res = self.output.stream.reserve(), if !pending_streams.is_empty() => {
                        match res {
                            Ok(permit) => {
                                permit.send(QuicStream::new(pending_streams.pop_front().unwrap(), self.ctrl.clone()));
                                worked = true;
                            }
                            Err(_) => return Err(Error::new(ErrorKind::BrokenPipe, "Stream channel closed")),
                        }
                    }
                }
            }

            // 5. --- 休眠等待 ---
            // 只有当这一轮什么都没干（没发包，没收包，没状态变更）时才睡觉
            if !worked {
                let sleep = match timeout {
                    None => false,
                    Some(deadline) => {
                        if deadline <= Instant::now() {
                            handle_timeout = true;
                            continue; // 直接进入下一轮循环处理超时
                        }
                        timer.as_mut().reset(deadline.into());
                        true
                    }
                };

                select! {
                    _ = self.ctrl.notify.notified() => {}, // 醒来，下一轮循环处理
                    _ = timer.as_mut(), if sleep => handle_timeout = true,
                }
            }
        }
    }
}
