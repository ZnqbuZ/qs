use anyhow::{anyhow, Result};
use bytes::BytesMut;
use quinn_plaintext::{client_config, server_config};
use quinn_proto::congestion::BbrConfig;
use quinn_proto::{
    ClientConfig, Connection, ConnectionEvent, ConnectionHandle, DatagramEvent, Dir, Endpoint,
    EndpointConfig, Event, ServerConfig, StreamEvent, Transmit, TransportConfig, VarInt,
    WriteError,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::select;
use tokio::sync::{mpsc, Notify};
use tokio::time::sleep_until;
use tracing::{error, info, trace, warn};
use tracing_subscriber::EnvFilter;

// 增加 source 字段模拟 IP 头
#[derive(Debug)]
struct NetPacket {
    source: SocketAddr,
    destination: SocketAddr,
    ecn: Option<quinn_proto::EcnCodepoint>,
    contents: Vec<u8>,
}

type NetTx = mpsc::Sender<NetPacket>;
type NetRx = mpsc::Receiver<NetPacket>;

// 1GB 数据量
const TOTAL_BYTES_TO_SEND: usize = 1024 * 1024 * 2048;
const CHUNK_SIZE: usize = 64 * 1024;

#[tokio::main]
async fn main() -> Result<()> {
    let filter = if cfg!(debug_assertions) {
        // Debug 构建，打印所有 debug / trace
        EnvFilter::new("qs=trace")
    } else {
        // Release 构建，只打印 info 以上
        EnvFilter::new("qs=info")
    };

    // 开启日志以便观察握手过程（可选）
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // 使用大 buffer 避免通道本身成为瓶颈，测试 quinn 的流控能力
    let (client_net_tx, server_net_rx) = mpsc::channel(2048);
    let (server_net_tx, client_net_rx) = mpsc::channel(2048);

    let server_addr: SocketAddr = "127.0.0.1:4433".parse()?;
    let client_addr: SocketAddr = "127.0.0.1:12345".parse()?;

    let mut server_config = server_config();
    server_config.transport = {
        let mut config = TransportConfig::default();
        
        config.initial_mtu(65535);
        config.min_mtu(65535);

        config.stream_receive_window(VarInt::from_u32(10 * 1024 * 1024));
        config.receive_window(VarInt::from_u32(15 * 1024 * 1024));

        config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
        config.max_concurrent_uni_streams(VarInt::from_u32(1024));

        config.congestion_controller_factory(Arc::new(BbrConfig::default()));

        config.keep_alive_interval(Some(Duration::from_secs(5)));
        config.max_idle_timeout(Some(VarInt::from_u32(30_000).into()));

        Arc::new(config)
    };

    let server_handle = tokio::spawn(run_server(
        server_config.clone(),
        server_addr,
        server_net_rx,
        server_net_tx,
    ));

    let client_handle = tokio::spawn(run_client(
        server_config.clone(),
        server_addr,
        client_addr,
        client_net_rx,
        client_net_tx,
    ));

    let _ = tokio::join!(server_handle, client_handle);
    Ok(())
}

async fn run_server(
    config: ServerConfig,
    local_addr: SocketAddr,
    mut net_rx: NetRx,
    net_tx: NetTx,
) -> Result<()> {
    let mut endpoint = Endpoint::new(
        Arc::new(EndpointConfig::default()),
        Some(Arc::new(config)),
        false,
        None,
    );

    let mut conn: Option<Connection> = None;
    let mut conn_handle: Option<ConnectionHandle> = None;
    let mut recv_buf = Vec::new();
    let mut received_bytes = 0;
    let mut start_time: Option<Instant> = None;

    info!("[Server] Started");

    loop {
        let mut did_work = false;

        // =========================================================================
        // 1. 收包阶段 (Ingress)
        // 尽可能多地读取网络包，避免每次只读一个就进入状态机处理
        // =========================================================================
        loop {
            // 注意：这里使用 try_recv 配合循环来实现 batch read
            match net_rx.try_recv() {
                Ok(packet) => {
                    did_work = true;
                    let now = Instant::now();
                    let payload = BytesMut::from(&packet.contents[..]);
                    recv_buf.clear();

                    let event = endpoint.handle(
                        now,
                        packet.source,
                        None,
                        packet.ecn,
                        payload,
                        &mut recv_buf,
                    );

                    match event {
                        Some(DatagramEvent::NewConnection(incoming)) => {
                            info!("[Server] Incoming connection");
                            // accept 可能会写回握手包到 recv_buf
                            let (handle, connection) =
                                endpoint.accept(incoming, now, &mut recv_buf, None).unwrap();
                            conn = Some(connection);
                            conn_handle = Some(handle);
                            if !recv_buf.is_empty() {
                                send_raw(&recv_buf, local_addr, packet.source, &net_tx).await;
                            }
                        }
                        Some(DatagramEvent::ConnectionEvent(h, event)) => {
                            if let Some(c) = conn.as_mut() {
                                if Some(h) == conn_handle {
                                    c.handle_event(event);
                                }
                            }
                        }
                        Some(DatagramEvent::Response(transmit)) => {
                            trace!("[Server] Sending response packet");
                            send_transmit(transmit, &recv_buf, local_addr, &net_tx).await;
                        }
                        None => {
                            if !recv_buf.is_empty() {
                                // 这里的 transmit 构造比较简单，直接原样发回去即可
                                // 注意：quinn-proto 的 buffer 可能包含多个 UDP 数据报，但在简单模拟中通常是一次 handle 一个
                                send_raw(&recv_buf, local_addr, packet.source, &net_tx).await;
                            }
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break, // 没包了，退出收包循环
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    error!("[Server] NetRx channel disconnected");
                    return Ok(());
                }
            }
        }

        // =========================================================================
        // 2. 状态机驱动阶段 (State Machine & IO)
        // =========================================================================
        if let Some(c) = conn.as_mut() {
            // A. 处理应用层事件 (Readable / Writable)
            while let Some(event) = c.poll() {
                did_work = true;
                match event {
                    Event::Connected { .. } => {
                        trace!("[Server] Connection established");
                    }

                    Event::Stream(StreamEvent::Readable { id }) => {
                        if start_time.is_none() {
                            start_time = Some(Instant::now());
                            info!("[Server] First byte received");
                        }

                        let mut stream = c.recv_stream(id);
                        // 关键：Readable 背压处理
                        // 必须一直读，直到读完或者 buffer 空，这样才能最大化释放流控窗口
                        if let Ok(mut chunks) = stream.read(true) {
                            while let Ok(Some(chunk)) = chunks.next(usize::MAX) {
                                received_bytes += chunk.bytes.len();
                            }
                            // 显式 finalize 确保状态更新
                            if chunks.finalize().should_transmit() {
                                // 这里的 should_transmit 其实不用手动处理，poll_transmit 会感知到
                            }
                        }
                    }
                    Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                        c.streams().accept(Dir::Bi); // 接受流
                    }
                    Event::ConnectionLost { reason } => {
                        error!("[Server] Lost: {:?}", reason);
                        return Ok(());
                    }
                    _ => {}
                }
            }

            // B. 发包阶段 (Egress)
            // 只要 quinn 还有包要发，就一直发，直到 drain 干净
            loop {
                recv_buf.clear();
                if let Some(transmit) = c.poll_transmit(Instant::now(), 1, &mut recv_buf) {
                    did_work = true;
                    send_transmit(transmit, &recv_buf, local_addr, &net_tx).await;
                } else {
                    break;
                }
            }

            // 检查完成
            if received_bytes >= TOTAL_BYTES_TO_SEND {
                let duration = start_time.unwrap().elapsed();
                let mb = received_bytes as f64 / 1024.0 / 1024.0;
                let speed = mb / duration.as_secs_f64();
                info!(
                    "[Server] Finished! {:.2} MB in {:.2}s. Speed: {:.2} MB/s ({:.2} Gbps)",
                    mb,
                    duration.as_secs_f64(),
                    speed,
                    speed * 8.0 / 1024.0
                );
                c.close(
                    Instant::now(),
                    VarInt::from_u32(0),
                    BytesMut::new().freeze(),
                );
                // 发送 Close frame
                while let Some(tx) = c.poll_transmit(Instant::now(), 1, &mut recv_buf) {
                    send_transmit(tx, &recv_buf, local_addr, &net_tx).await;
                }
                return Ok(());
            }
        }

        // =========================================================================
        // 3. 休眠阶段 (Select)
        // 只有在上一轮什么都没做 (did_work == false) 时才睡觉
        // =========================================================================
        if !did_work {
            let timeout = conn.as_mut().and_then(|c| c.poll_timeout());
            select! {
                // 等网络包
                res = net_rx.recv() => {
                    match res {
                        Some(packet) => {
                             // 这里我们可以简单地把包放回去处理，或者直接在这里处理。
                             // 为了逻辑复用，我们这里不做重逻辑，只是用来唤醒 loop。
                             // 但因为 net_rx 是 queue，我们已经 pop 出来了，必须处理。
                             let now = Instant::now();
                             let payload = BytesMut::from(&packet.contents[..]);
                             recv_buf.clear();
                             let event = endpoint.handle(now, packet.source, None, packet.ecn, payload, &mut recv_buf);

                             // 稍微有点重复代码，但为了结构清晰忍了，或者封装个 handle_packet 函数
                             match event {
                                 Some(DatagramEvent::ConnectionEvent(h, e)) => {
                                     if let Some(c) = conn.as_mut() {
                                         if Some(h) == conn_handle { c.handle_event(e); }
                                     }
                                 }
                                 Some(DatagramEvent::Response(transmit)) => {
                                     send_transmit(transmit, &recv_buf, local_addr, &net_tx).await;
                                 }
                                 Some(DatagramEvent::NewConnection(incoming)) => {
                            info!("[Server] Incoming connection");
                            // accept 可能会写回握手包到 recv_buf
                            let (handle, connection) = endpoint.accept(incoming, now, &mut recv_buf, None).unwrap();
                            conn = Some(connection);
                            conn_handle = Some(handle);
                            if !recv_buf.is_empty() {
                                send_raw(&recv_buf, local_addr, packet.source, &net_tx).await;
                            }
                                } // Server 运行时一般不会再次 NewConnection
                                 None => {}
                             }
                        }
                        None => return Ok(()),
                    }
                }
                // 等超时
                _ = sleep_until_opt(timeout), if timeout.is_some() => {
                    if let Some(c) = conn.as_mut() {
                        c.handle_timeout(Instant::now());
                    }
                }
            }
        }
    }
}

async fn run_client(
    config: ServerConfig,
    server_addr: SocketAddr,
    local_addr: SocketAddr,
    mut net_rx: NetRx,
    net_tx: NetTx,
) -> Result<()> {
    let mut endpoint = Endpoint::new(
        Arc::new(EndpointConfig::default()),
        Some(Arc::new(config)),
        false,
        None,
    );

    let (conn_handle, mut conn) =
        endpoint.connect(Instant::now(), client_config(), server_addr, "localhost")?;
    let mut recv_buf = Vec::new();
    let data_chunk = vec![0u8; CHUNK_SIZE];

    // 状态追踪
    let mut bytes_sent = 0;
    let mut stream_id = None;
    let mut stream_finished = false;

    // 初始启动
    info!("[Client] Started, connecting to {}", server_addr);

    loop {
        let mut did_work = false;

        // 1. 收包 (Ingress)
        loop {
            match net_rx.try_recv() {
                Ok(packet) => {
                    did_work = true;
                    let now = Instant::now();
                    let payload = BytesMut::from(&packet.contents[..]);
                    recv_buf.clear();
                    let event = endpoint.handle(
                        now,
                        packet.source,
                        None,
                        packet.ecn,
                        payload,
                        &mut recv_buf,
                    );

                    if let Some(DatagramEvent::ConnectionEvent(h, e)) = event {
                        if h == conn_handle {
                            conn.handle_event(e);
                        }
                    } else if let Some(DatagramEvent::Response(transmit)) = event {
                        send_transmit(transmit, &recv_buf, local_addr, &net_tx).await;
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(_) => return Ok(()),
            }
        }

        trace!("[Client] After Ingress: bytes_sent = {}", bytes_sent);

        // 2. 状态机 (State Machine)
        while let Some(event) = conn.poll() {
            did_work = true;
            match event {
                Event::Connected => {
                    info!("[Client] Connected");
                    stream_id = Some(conn.streams().open(Dir::Bi).unwrap());
                }
                Event::Stream(StreamEvent::Writable { id }) => {
                    // 关键：Writable 背压处理
                    // 只要 Writable 了，就说明有窗口了，死命写，直到写到 Blocked 为止
                    if Some(id) == stream_id && !stream_finished {
                        if try_send_data(
                            &mut conn,
                            id,
                            &data_chunk,
                            &mut bytes_sent,
                            &mut stream_finished,
                        ) {
                            // 这里的返回值 bool 暂时没用，逻辑在内部处理了
                        }
                    }
                }
                Event::ConnectionLost { .. } => return Ok(()),
                _ => {}
            }
        }

        trace!("[Client] After State Machine: bytes_sent = {}", bytes_sent);

        // 3. 主动尝试写 (App Logic)
        // 如果我们处于连接状态且流还没发完，尝试去写。
        // 这对于初始状态或某些边缘触发丢失的情况很重要。
        if let Some(id) = stream_id {
            if !stream_finished {
                // 如果成功写入了数据（哪怕一点点），都算 did_work，因为这可能触发了需要发包
                if try_send_data(
                    &mut conn,
                    id,
                    &data_chunk,
                    &mut bytes_sent,
                    &mut stream_finished,
                ) {
                    did_work = true;
                }
            }
        }

        // 4. 发包 (Egress)
        loop {
            recv_buf.clear();
            if let Some(transmit) = conn.poll_transmit(Instant::now(), 1, &mut recv_buf) {
                did_work = true;
                send_transmit(transmit, &recv_buf, local_addr, &net_tx).await;
            } else {
                break;
            }
        }

        // 5. 休眠
        if !did_work {
            let timeout = conn.poll_timeout();
            select! {
                res = net_rx.recv() => {
                    if let Some(packet) = res {
                        // 处理逻辑同上，只做唤醒后的单次处理，Loop 会负责后续 Batch
                        let now = Instant::now();
                        recv_buf.clear();
                        let event = endpoint.handle(now, packet.source, None, packet.ecn, BytesMut::from(&packet.contents[..]), &mut recv_buf);
                        if let Some(DatagramEvent::ConnectionEvent(h, e)) = event {
                            if h == conn_handle { conn.handle_event(e); }
                        } else if let Some(DatagramEvent::Response(transmit)) = event {
                            send_transmit(transmit, &recv_buf, local_addr, &net_tx).await;
                        }
                    } else {
                        return Ok(());
                    }
                }
                _ = sleep_until_opt(timeout), if timeout.is_some() => {
                    conn.handle_timeout(Instant::now());
                }
            }
        }
    }
}

// 返回 true 表示写进去了一些数据， false 表示一点都没写进去（Blocked）
fn try_send_data(
    conn: &mut Connection,
    id: quinn_proto::StreamId,
    chunk: &[u8],
    bytes_sent: &mut usize,
    finished: &mut bool,
) -> bool {
    let mut writer = conn.send_stream(id);
    let mut wrote_something = false;

    while *bytes_sent < TOTAL_BYTES_TO_SEND {
        trace!("[Client] Trying to send data: bytes_sent = {}", bytes_sent);
        let remaining = TOTAL_BYTES_TO_SEND - *bytes_sent;
        let to_write = std::cmp::min(remaining, chunk.len());
        let is_fin = *bytes_sent + to_write == TOTAL_BYTES_TO_SEND;

        match writer.write(chunk.get(..to_write).unwrap()) {
            Ok(bytes) => {
                *bytes_sent += bytes;
                wrote_something = true;
                if is_fin {
                    let _ = writer.finish();
                    *finished = true;
                    info!("[Client] All data sent!");
                    break;
                }
            }
            Err(WriteError::Blocked) => {
                // 背压生效：窗口满了，停止写入，等待 Writable 事件
                break;
            }
            Err(e) => {
                error!("Write error: {:?}", e);
                *finished = true;
                break;
            }
        }
    }
    wrote_something
}

async fn send_raw(buf: &[u8], source: SocketAddr, destination: SocketAddr, tx: &NetTx) {
    if let Err(e) = tx
        .send(NetPacket {
            source,
            destination,
            ecn: None,
            contents: Vec::from(buf),
        })
        .await
    {
        error!("Send raw error: {:?}", e);
    }
}

async fn send_transmit(transmit: Transmit, recv_buf: &[u8], source: SocketAddr, tx: &NetTx) {
    if let Err(e) = tx
        .send(NetPacket {
            source,
            destination: transmit.destination,
            ecn: transmit.ecn,
            contents: Vec::from(&recv_buf[..transmit.size]),
        })
        .await
    {
        error!("Send transmit error: {:?}", e);
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    if let Some(d) = deadline {
        sleep_until(d.into()).await;
    } else {
        std::future::pending::<()>().await;
    }
}
