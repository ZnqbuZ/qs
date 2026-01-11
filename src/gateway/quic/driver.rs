use crate::gateway::quic::cmd::QuicCmd;
use crate::gateway::quic::evt::{QuicNetEvt, QuicNetEvtTx, QuicStreamEvt, QuicStreamEvtTx};
use crate::gateway::quic::packet::{QuicPacket, QuicPacketMargins};
use crate::gateway::quic::stream::{QuicStreamCtx, QuicStreamFlowCtrl, QuicStreamHdl};
use crate::gateway::quic::utils::QuicBufferPool;
use crate::gateway::quic::{SwitchedReceiver, SwitchedSender};
use anyhow::{anyhow, Error};
use bytes::Bytes;
use derive_more::{Deref, DerefMut, From, Into};
use quinn_proto::{
    ClientConfig, ConnectError, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, Event,
    ReadError, ReadableError, StreamEvent, StreamId, WriteError,
};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use futures::task::AtomicWaker;
use tokio::sync::mpsc;
use tracing::{error, trace, warn};

const QUIC_STREAM_EVT_BUFFER: usize = 500_000;
const QUIC_PACKET_POOL_MIN_CAPACITY: usize = 64 * 1024;

pub type QuicStreamCtxTx = SwitchedSender<QuicStreamCtx>;
pub type QuicStreamCtxRx = SwitchedReceiver<QuicStreamCtx>;

#[derive(Debug, Deref, DerefMut)]
struct QuicStreamWritePending {
    #[deref]
    #[deref_mut]
    chunks: VecDeque<Bytes>,
    fin: bool,
}

impl QuicStreamWritePending {
    fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            fin: false,
        }
    }
}

#[derive(Debug)]
struct QuicStreamDrvCtx {
    tx: QuicStreamEvtTx,
    ctrl: Arc<QuicStreamFlowCtrl>,
    pending: QuicStreamWritePending,
    read_pending: VecDeque<Bytes>,
    read_blocked: Arc<AtomicBool>,
}

pub(crate) struct QuicDriver {
    conns: HashMap<ConnectionHandle, (Connection, HashMap<StreamId, QuicStreamDrvCtx>)>,
    endpoint: Endpoint,
    client_config: ClientConfig,
    net_evt_tx: QuicNetEvtTx,
    incoming_stream_tx: QuicStreamCtxTx,
    buf: Vec<u8>,
    packet_pool: QuicBufferPool,
    packet_margins: QuicPacketMargins,
}

impl QuicDriver {
    pub fn new(
        endpoint: Endpoint,
        client_config: ClientConfig,
        net_evt_tx: QuicNetEvtTx,
        incoming_stream_tx: QuicStreamCtxTx,
        packet_margins: QuicPacketMargins,
    ) -> Self {
        Self {
            conns: HashMap::new(),
            endpoint,
            client_config,
            net_evt_tx,
            incoming_stream_tx,
            buf: Vec::with_capacity(64 * 1024),
            packet_pool: QuicBufferPool::new(QUIC_PACKET_POOL_MIN_CAPACITY),
            packet_margins,
        }
    }

    fn read_stream_from_quinn(conn: &mut Connection, id: StreamId, ctx: &mut QuicStreamDrvCtx) {
        // 如果还有积压数据没发出去，绝对不要读新的
        if !ctx.read_pending.is_empty() {
            return;
        }

        let mut stream = conn.recv_stream(id);
        let mut chunks = match stream.read(true) {
            Ok(chunks) => chunks,
            Err(e) => {
                if !matches!(e, ReadableError::ClosedStream) {
                    error!("Stream is not readable: {:?}", e);
                }
                return;
            }
        };

        loop {
            match chunks.next(usize::MAX) {
                Ok(Some(chunk)) => {
                    match ctx.tx.try_send(QuicStreamEvt::Data(chunk.bytes)) {
                        Ok(_) => {
                            // 发送成功，继续读
                        }
                        Err(mpsc::error::TrySendError::Full(evt)) => {
                            error!("Stream channel is full");
                            // 1. 保存当前被退回的 chunk
                            if let QuicStreamEvt::Data(bytes) = evt {
                                ctx.read_pending.push_back(bytes);
                            }

                            // === 修复开始 ===
                            // 2. 必须把 chunks 迭代器中剩余的所有数据都读出来放入 pending
                            // 否则这些数据会随 chunks drop 而丢失
                            while let Ok(Some(remaining_chunk)) = chunks.next(usize::MAX) {
                                ctx.read_pending.push_back(remaining_chunk.bytes);
                            }
                            // === 修复结束 ===

                            ctx.read_blocked.store(true, Ordering::Relaxed);
                            break;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            break;
                        }
                    }
                }

                // ... 其他分支保持不变 ...
                Ok(None) => {
                    if let Err(e) = ctx.tx.try_send(QuicStreamEvt::Fin) {
                        error!("Failed to send fin to stream: {:?}", e);
                    }
                    break;
                }
                Err(e) => {
                    if let ReadError::Reset(code) = e {
                        let _ = ctx.tx.try_send(QuicStreamEvt::Reset(format!(
                            "Failed to read from stream. Error code: {code}"
                        )));
                    }
                    break;
                }
            }
        }
    }

    // TODO: add more commands
    pub fn execute(&mut self, cmd: QuicCmd) {
        match cmd {
            QuicCmd::InputPacket(packet) => {
                self.handle_packet_input(packet);
            }

            QuicCmd::OpenBiStream {
                addr,
                data,
                stream_tx,
            } => {
                if let Err(e) = stream_tx.send(self.open_stream(addr, Dir::Bi, data)) {
                    error!("Failed to send opened stream: {:?}", e);
                }
            }

            QuicCmd::StreamWrite {
                stream_hdl,
                data,
                fin,
            } => {
                self.write_stream(stream_hdl, data, fin);
            }

            // 新增：处理反压解除
            QuicCmd::StreamUnblockRead { stream_hdl } => {
                trace!("StreamUnblockRead received for {:?}", stream_hdl);
                if let Some((conn, streams)) = self.conns.get_mut(&stream_hdl.conn_hdl) {
                    if let Some(ctx) = streams.get_mut(&stream_hdl.stream_id) {
                        // 1. 尝试清空积压的数据
                        let mut flushed = true;
                        while let Some(data) = ctx.read_pending.front() {
                            match ctx.tx.try_send(QuicStreamEvt::Data(data.clone())) {
                                Ok(_) => {
                                    ctx.read_pending.pop_front();
                                }
                                Err(_) => {
                                    flushed = false;
                                    // 仍然满，设置标志，等待下一次 Unblock
                                    ctx.read_blocked.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                        }

                        // 2. 如果积压已清空，说明通道有空位，主动从 quinn 拉取更多数据
                        if flushed {
                            Self::read_stream_from_quinn(conn, stream_hdl.stream_id, ctx);
                        }
                    }
                }
            }

            QuicCmd::ResetStream {
                stream_hdl,
                error_code,
            } => {
                if let Some((conn, _)) = self.conns.get_mut(&stream_hdl.conn_hdl) {
                    let _ = conn
                        .send_stream(stream_hdl.stream_id)
                        .reset(error_code.into());
                    self.process_conn(stream_hdl.conn_hdl);
                }
            }

            QuicCmd::StopStream {
                stream_hdl,
                error_code,
            } => {
                if let Some((conn, _)) = self.conns.get_mut(&stream_hdl.conn_hdl) {
                    let _ = conn
                        .recv_stream(stream_hdl.stream_id)
                        .stop(error_code.into());
                    self.process_conn(stream_hdl.conn_hdl);
                }
            }

            _ => {}
        }
    }
}

macro_rules! emit_transmit {
    ($drv:expr, $transmit:expr) => {{
        $drv.net_evt_tx
            .try_send(QuicNetEvt::OutputPacket(QuicPacket::new(
                $transmit.destination,
                $drv.packet_pool
                    .buf(&$drv.buf[0..$transmit.size], $drv.packet_margins),
            )))
    }};
}

impl QuicDriver {
    fn handle_packet_input(&mut self, packet: QuicPacket) {
        let now = Instant::now();

        self.buf.clear();
        match self
            .endpoint
            .handle(now, packet.addr, None, None, packet.payload, &mut self.buf)
        {
            Some(DatagramEvent::NewConnection(incoming)) => {
                trace!("New connection from {:?}", incoming.remote_address());

                if !self.incoming_stream_tx.switch.get() {
                    trace!("Incoming stream channel is closed. Connection dropped.");
                    return;
                }

                match self.endpoint.accept(incoming, now, &mut self.buf, None) {
                    Ok((conn_hdl, conn)) => {
                        trace!("Accepted connection {:?}", conn_hdl);
                        self.conns.insert(conn_hdl, (conn, HashMap::new()));
                        self.process_conn(conn_hdl);
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {:?}", e);
                    }
                }
            }

            Some(DatagramEvent::ConnectionEvent(conn_hdl, event)) => {
                if let Some((conn, _)) = self.conns.get_mut(&conn_hdl) {
                    conn.handle_event(event);
                    self.process_conn(conn_hdl);
                }
            }

            Some(DatagramEvent::Response(transmit)) => {
                if let Err(e) = emit_transmit!(self, transmit) {
                    error!("Failed to send response packet: {:?}", e);
                }
            }

            None => {}
        }
    }

    fn connect(&mut self, addr: SocketAddr) -> Result<ConnectionHandle, ConnectError> {
        if let Some((conn_hdl, _)) = self
            .conns
            .iter()
            .find(|(_, (conn, _))| conn.remote_address() == addr)
        {
            return Ok(*conn_hdl);
        }

        let (conn_hdl, conn) =
            self.endpoint
                .connect(Instant::now(), self.client_config.clone(), addr, "")?;
        self.conns.insert(conn_hdl, (conn, HashMap::new()));
        self.process_conn(conn_hdl);
        Ok(conn_hdl)
    }

    fn new_stream_ctx(stream_hdl: QuicStreamHdl) -> (QuicStreamCtx, QuicStreamDrvCtx) {
        let (tx, rx) = mpsc::channel(QUIC_STREAM_EVT_BUFFER);
        let ctrl: Arc<_> = QuicStreamFlowCtrl::new().into();
        let read_blocked = Arc::new(AtomicBool::new(false));
        (
            QuicStreamCtx {
                hdl: stream_hdl,
                rx,
                ctrl: ctrl.clone(),
                read_blocked: read_blocked.clone(),
            },
            QuicStreamDrvCtx {
                tx,
                ctrl,
                pending: QuicStreamWritePending::new(),
                read_pending: VecDeque::new(),
                read_blocked
            },
        )
    }

    fn open_stream(
        &mut self,
        addr: SocketAddr,
        dir: Dir,
        data: Option<Bytes>,
    ) -> Result<QuicStreamCtx, Error> {
        let conn_hdl = self.connect(addr)?;
        let (conn, streams) = self
            .conns
            .get_mut(&conn_hdl)
            .ok_or_else(|| anyhow!("Failed to find connection {:?}", conn_hdl))?;
        let stream_id = conn
            .streams()
            .open(dir)
            .ok_or_else(|| anyhow!("Failed to open stream"))?;

        let stream_hdl: QuicStreamHdl = (conn_hdl, stream_id).into();
        let (ctx, drv_ctx) = Self::new_stream_ctx(stream_hdl);
        if !conn.is_handshaking() {
            drv_ctx.tx.try_send(QuicStreamEvt::Ready)?;
        }
        streams.insert(stream_id, drv_ctx);

        if let Some(data) = data {
            self.write_stream(stream_hdl, data, false);
        }

        Ok(ctx)
    }

    fn write_stream(&mut self, stream_hdl: QuicStreamHdl, data: Bytes, fin: bool) {
        let conn_hdl = stream_hdl.conn_hdl;

        let (conn, ctx) = if let Some((conn, streams)) = self.conns.get_mut(&conn_hdl) {
            if let Some(ctx) = streams.get_mut(&stream_hdl.stream_id) {
                (conn, ctx)
            } else {
                warn!("write_stream ignored: stream {:?} not found", stream_hdl);
                return;
            }
        } else {
            warn!("write_stream ignored: connection {:?} not found", conn_hdl);
            return;
        };

        if ctx.ctrl.blocked.load(Ordering::Acquire) {
            trace!("Stream {:?} is blocked. Buffering data.", stream_hdl);
            ctx.pending.push_back(data);
            if fin {
                ctx.pending.fin = true;
            }
            let id = stream_hdl.stream_id;
            let mut stream = conn.send_stream(id);
            let mut flushed = false;
            let pending = &mut ctx.pending;
            loop {
                let chunks = pending.make_contiguous();
                match stream.write_chunks(chunks) {
                    Ok(written) => {
                        pending.drain(..written.chunks);
                        trace!(
                                        "Stream {:?} wrote {} bytes, {} chunks remaining",
                                        id,
                                        written.bytes,
                                        pending.len()
                                    );
                        if pending.is_empty() {
                            flushed = true;
                            break;
                        }
                    }
                    Err(WriteError::Blocked) => {
                        trace!("Stream {:?} still blocked after write attempt", id);
                        break;
                    }
                    Err(e) => {
                        error!("Stream {:?} write error: {:?}", id, e);
                        flushed = true;
                        break;
                    }
                }
            }
            if flushed {
                if pending.fin {
                    if let Err(e) = stream.finish() {
                        error!("Failed to finish stream {:?}: {:?}", id, e);
                    }
                }
                ctx.ctrl.blocked.store(false, Ordering::Release);
                ctx.ctrl.waker.wake();
            }
        } else {
            let mut stream = conn.send_stream(stream_hdl.stream_id);
            let len = data.len();
            let mut chunks = vec![data];

            match stream.write_chunks(&mut chunks) {
                Ok(written) if written.bytes == len => {
                    if fin {
                        if let Err(e) = stream.finish() {
                            error!("Failed to finish stream {:?}: {:?}", stream_hdl, e);
                        }
                    }
                }

                Ok(_) | Err(WriteError::Blocked) => {
                    ctx.ctrl.blocked.store(true, Ordering::Release);
                    for chunk in chunks.drain(..) {
                        ctx.pending.push_back(chunk);
                    }
                    if fin {
                        ctx.pending.fin = true;
                    }
                }

                Err(e) => {
                    error!("Failed to write to stream {:?}: {:?}", stream_hdl, e);
                    let _ = stream.reset(0u32.into());
                }
            }
        }

        self.process_conn(conn_hdl);
    }
}

impl QuicDriver {
    fn process_conn(&mut self, conn_hdl: ConnectionHandle) {
        let mut rm_conn = false;

        let (conn, streams) = match self.conns.get_mut(&conn_hdl) {
            Some(c) => c,
            None => return,
        };

        while let Some(evt) = conn.poll() {
            match evt {
                Event::Connected => {
                    trace!("Connection established {:?}", conn_hdl);
                    for ctx in streams.values() {
                        let _ = ctx.tx.try_send(QuicStreamEvt::Ready);
                    }
                }

                Event::ConnectionLost { reason } => {
                    error!("Connection lost: {:?}", reason);
                    rm_conn = true;
                    for ctx in streams.values() {
                        let _ = ctx.tx.try_send(QuicStreamEvt::Reset(format!(
                            "Connection lost: {:?}",
                            reason.to_string()
                        )));
                    }
                }

                Event::Stream(stream_evt) => match stream_evt {
                    StreamEvent::Opened { dir } => {
                        while let Some(stream_id) = conn.streams().accept(dir) {
                            trace!(
                                "Accepted new stream: {:?} on connection {:?}",
                                stream_id,
                                conn_hdl
                            );

                            let (ctx, drv_ctx) = Self::new_stream_ctx((conn_hdl, stream_id).into());
                            if let Err(e) = self.incoming_stream_tx.try_send(ctx) {
                                error!("Failed to hand off stream: {:?}", e);
                            } else {
                                streams.insert(stream_id, drv_ctx);
                            }
                        }
                    }

                    StreamEvent::Readable { id } => {
                        // trace!("Stream {:?} is readable", id);
                        if let Some(ctx) = streams.get_mut(&id) {
                            Self::read_stream_from_quinn(conn, id, ctx);
                        }
                    }

                    StreamEvent::Writable { id } => {
                        trace!("Stream {:?} is writable", id);
                        if let Some(ctx) = streams.get_mut(&id) {
                            let mut stream = conn.send_stream(id);
                            let mut flushed = false;
                            let pending = &mut ctx.pending;
                            loop {
                                let chunks = pending.make_contiguous();
                                match stream.write_chunks(chunks) {
                                    Ok(written) => {
                                        pending.drain(..written.chunks);
                                        trace!(
                                        "Stream {:?} wrote {} bytes, {} chunks remaining",
                                        id,
                                        written.bytes,
                                        pending.len()
                                    );
                                        if pending.is_empty() {
                                            flushed = true;
                                            break;
                                        }
                                    }
                                    Err(WriteError::Blocked) => {
                                        trace!("Stream {:?} still blocked after write attempt", id);
                                        break;
                                    }
                                    Err(e) => {
                                        error!("Stream {:?} write error: {:?}", id, e);
                                        flushed = true;
                                        break;
                                    }
                                }
                            }
                            if flushed {
                                if pending.fin {
                                    if let Err(e) = stream.finish() {
                                        error!("Failed to finish stream {:?}: {:?}", id, e);
                                    }
                                }
                                ctx.ctrl.blocked.store(false, Ordering::Release);
                                ctx.ctrl.waker.wake();
                            }
                        }
                    }

                    StreamEvent::Stopped { id, error_code } => {
                        if let Some(ctx) = streams.get_mut(&id) {
                            let _ = ctx.tx.try_send(QuicStreamEvt::Reset(format!(
                                "Remote stop: {error_code}"
                            )));
                        }
                    }

                    _ => {
                        trace!("Unhandled stream event: {:?}", stream_evt);
                    }
                },

                _ => {
                    trace!("Unhandled connection event: {:?}", evt);
                }
            }
        }

        let now = Instant::now();
        loop {
            self.buf.clear();
            if let Some(transmit) = conn.poll_transmit(now, 1, &mut self.buf) {
                if let Err(e) = emit_transmit!(self, transmit) {
                    error!("Failed to send transmit packet: {:?}", e);
                }
            } else {
                break;
            }
        }

        if rm_conn {
            self.conns.remove(&conn_hdl);
        }
    }
}

impl QuicDriver {
    pub fn handle_timeout(&mut self) {
        trace!("Handling timeouts");

        let now = Instant::now();

        let expired_handles: Vec<_> = self
            .conns
            .iter_mut()
            .filter_map(|(conn_hdl, (conn, _))| {
                conn.poll_timeout()
                    .and_then(|t| if t <= now { Some(*conn_hdl) } else { None })
            })
            .collect();

        for conn_hdl in expired_handles {
            if let Some((conn, _)) = self.conns.get_mut(&conn_hdl) {
                conn.handle_timeout(now);
            }

            self.process_conn(conn_hdl);
        }
    }

    pub fn min_timeout(&mut self) -> Option<Instant> {
        self.conns
            .values_mut()
            .filter_map(|(conn, _)| conn.poll_timeout())
            .min()
    }
}
