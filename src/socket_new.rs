use atomic_refcell::AtomicRefCell;
use bytes::BytesMut;
use derive_more::{Constructor, From, Into};
use quinn::{
    udp::{EcnCodepoint, RecvMeta, Transmit}, AsyncUdpSocket,
    UdpPoller,
};
use std::cell::RefCell;
use std::cmp::max;
use std::ptr::copy_nonoverlapping;
use std::{
    fmt::Debug,
    io::IoSliceMut,
    net::SocketAddr,
    ops::DerefMut,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::PollSender;
use tracing::{trace, warn};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

//region packet
#[derive(Debug, Constructor)]
struct QuicPacket {
    addr: SocketAddr,
    payload: BytesMut,
    ecn: Option<EcnCodepoint>,
}
//endregion

//region utils
#[derive(Debug, Clone, Copy, From, Into)]
pub struct BufMargins {
    pub header: usize,
    pub trailer: usize,
}

impl BufMargins {
    pub fn len(&self) -> usize {
        self.header + self.trailer
    }
}

#[derive(Debug)]
pub(super) struct BufPool {
    pool: BytesMut,
    min_capacity: usize,
}

impl BufPool {
    #[inline]
    fn new(min_capacity: usize) -> Self {
        Self {
            pool: BytesMut::new(),
            min_capacity,
        }
    }

    fn buf(&mut self, data: &[u8], margins: BufMargins) -> BytesMut {
        let len = margins.len() + data.len();

        if len > self.pool.capacity() {
            let additional = max(len * 4, self.min_capacity);
            self.pool.reserve(additional);
            unsafe {
                self.pool.set_len(self.pool.capacity());
            }
        }

        let mut buf = self.pool.split_to(len);
        let (header, trailer) = margins.into();
        buf[header..len - trailer].copy_from_slice(data);
        buf
    }
}
//endregion

//region socket
#[derive(Debug)]
struct QuicSocketPoller {
    tx: PollSender<QuicPacket>,
}

impl UdpPoller for QuicSocketPoller {
    fn poll_writable(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> Poll<std::io::Result<()>> {
        self.get_mut()
            .tx
            .poll_reserve(cx)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
    }
}

#[derive(Debug)]
pub struct QuicSocket {
    addr: SocketAddr,
    rx: AtomicRefCell<Receiver<QuicPacket>>,
    tx: Sender<QuicPacket>,
    pool: AtomicRefCell<BufPool>,
    margins: BufMargins,
}

impl AsyncUdpSocket for QuicSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::into_pin(Box::new(QuicSocketPoller {
            tx: PollSender::new(self.tx.clone()),
        }))
    }

    fn try_send(&self, transmit: &Transmit) -> std::io::Result<()> {
        match transmit.destination {
            SocketAddr::V4(addr) => {
                let len = transmit.contents.len();
                trace!("{:?} sending {:?} bytes to {:?}", self.addr, len, addr);

                let segment_size = transmit.segment_size.unwrap_or(len);

                for chunk in transmit.contents.chunks(segment_size) {
                    let len = chunk.len();
                    let payload_len = len + self.margins.len();
                    let mut payload = BytesMut::with_capacity(payload_len);
                    unsafe {
                        payload.set_len(payload_len);
                        copy_nonoverlapping(chunk.as_ptr(), payload.as_mut_ptr(), len);
                    }

                    self.tx
                        .try_send(QuicPacket {
                            addr: transmit.destination,
                            payload,
                            ecn: transmit.ecn,
                        })
                        .map_err(|e| match e {
                            TrySendError::Full(_) => std::io::ErrorKind::WouldBlock,
                            TrySendError::Closed(_) => std::io::ErrorKind::BrokenPipe,
                        })?;
                }

                Ok(())
            }
            _ => Err(std::io::ErrorKind::ConnectionRefused.into()),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut std::task::Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        if bufs.is_empty() || meta.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let mut rx = self.rx.borrow_mut();
        let mut count = 0;

        for (buf, meta) in bufs.iter_mut().zip(meta.iter_mut()) {
            match rx.poll_recv(cx) {
                Poll::Ready(Some(packet)) => {
                    let len = packet.payload.len();
                    if len > buf.len() {
                        warn!(
                            "buffer too small for packet: {:?} < {:?}, dropped",
                            buf.len(),
                            len,
                        );
                        continue;
                    }
                    trace!(
                        "{:?} received {:?} bytes from {:?}",
                        self.addr, len, packet.addr
                    );
                    buf[0..len].copy_from_slice(&packet.payload);
                    *meta = RecvMeta {
                        addr: packet.addr,
                        len,
                        stride: len,
                        ecn: packet.ecn,
                        dst_ip: None,
                    };
                    count += 1;
                }
                Poll::Ready(None) if count > 0 => break,
                Poll::Ready(None) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "socket closed",
                    )));
                }
                Poll::Pending => break,
            }
        }

        if count > 0 {
            Poll::Ready(Ok(count))
        } else {
            Poll::Pending
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quinn::{ClientConfig, Endpoint, EndpointConfig, ServerConfig};
    use quinn_plaintext::{client_config, server_config};
    use quinn_proto::congestion::BbrConfig;
    use quinn_proto::{TransportConfig, VarInt};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tracing::info;

    fn init() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .try_init();
    }

    /// 辅助函数：创建一对相互连接的 QuicSocket
    /// socket_a 发送的数据会进入 socket_b 的 rx，反之亦然。
    fn make_socket_pair() -> (QuicSocket, QuicSocket) {
        let addr_a: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:5001".parse().unwrap();

        // 两个方向的通道：A->B 和 B->A
        // 容量给够，防止高并发时丢包
        let (tx_a_out, rx_a_out) = mpsc::channel::<QuicPacket>(1 << 32);
        let (tx_b_in, rx_b_in) = mpsc::channel::<QuicPacket>(1 << 32);

        let (tx_b_out, rx_b_out) = mpsc::channel::<QuicPacket>(1 << 32);
        let (tx_a_in, rx_a_in) = mpsc::channel::<QuicPacket>(1 << 32);

        forward(rx_a_out, tx_b_in, addr_a);
        forward(rx_b_out, tx_a_in, addr_b);

        let socket_a = QuicSocket {
            addr: addr_a,
            rx: AtomicRefCell::new(rx_a_in),
            tx: tx_a_out,
            pool: AtomicRefCell::new(BufPool::new(1024 * 1024 * 1024)),
            margins: (0, 0).into(),
        };

        let socket_b = QuicSocket {
            addr: addr_b,
            rx: AtomicRefCell::new(rx_b_in),
            tx: tx_b_out,
            pool: AtomicRefCell::new(BufPool::new(1024 * 1024 * 1024)),
            margins: (0, 0).into(),
        };

        (socket_a, socket_b)
    }

    fn config() -> (EndpointConfig, ServerConfig, ClientConfig) {
        let mut transport_config = TransportConfig::default();

        transport_config.initial_mtu(65535);
        transport_config.min_mtu(65535);

        transport_config.stream_receive_window(VarInt::from_u32(10 * 1024 * 1024));
        transport_config.receive_window(VarInt::from_u32(15 * 1024 * 1024));

        transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
        transport_config.max_concurrent_uni_streams(VarInt::from_u32(1024));

        transport_config.datagram_receive_buffer_size(None);
        transport_config.datagram_send_buffer_size(usize::MAX);

        transport_config.congestion_controller_factory(Arc::new(BbrConfig::default()));

        transport_config.keep_alive_interval(Some(Duration::from_secs(15)));
        transport_config.max_idle_timeout(Some(VarInt::from_u32(3_000).into()));

        transport_config.enable_segmentation_offload(true);

        transport_config.mtu_discovery_config(None);

        let transport_config = Arc::new(transport_config);

        let mut server_config = server_config();
        server_config.transport = transport_config.clone();

        let mut client_config = client_config();
        client_config.transport_config(transport_config.clone());

        let mut endpoint_config = EndpointConfig::default();
        endpoint_config.max_udp_payload_size(65507).unwrap();

        (endpoint_config, server_config, client_config)
    }

    fn endpoint() -> (Endpoint, Endpoint) {
        let (endpoint_config, server_config, client_config) = config();

        // 1. 创建内存 Socket 对
        let (socket_client, socket_server) = make_socket_pair();
        let socket_client = Arc::new(socket_client);
        let socket_server = Arc::new(socket_server);

        // 3. 配置 Client Endpoint
        let mut client_endpoint = Endpoint::new_with_abstract_socket(
            endpoint_config.clone(),
            Some(server_config.clone()),
            socket_client.clone(),
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();
        client_endpoint.set_default_client_config(client_config.clone());

        // 2. 配置 Server Endpoint
        let mut server_endpoint = Endpoint::new_with_abstract_socket(
            endpoint_config.clone(),
            Some(server_config.clone()),
            socket_server.clone(),
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();
        server_endpoint.set_default_client_config(client_config.clone());

        (client_endpoint, server_endpoint)
    }

    fn forward(mut rx: Receiver<QuicPacket>, tx: Sender<QuicPacket>, addr: SocketAddr) {
        const BATCH_SIZE: usize = 1024;
        tokio::spawn(async move {
            // 关键优化：使用 buffer 批量处理
            let mut buffer = Vec::with_capacity(BATCH_SIZE);

            // recv_many 会在有数据时唤醒，一次最多拿 100 个包
            // 这比每次拿 1 个包减少了 99 次上下文切换开销
            while rx.recv_many(&mut buffer, BATCH_SIZE).await > 0 {
                for packet in buffer.iter_mut() {
                    // 【过滤逻辑】：在此处修改地址
                    packet.addr = addr;
                }
                // 批量转发
                for packet in buffer.drain(..) {
                    if let Err(e) = tx.send(packet).await {
                        info!("{:?}", e);
                        return; // 通道已关闭
                    }
                }
            }
        });
    }

    // #[tokio::test(flavor = "multi_thread")]
    async fn test_ping() -> anyhow::Result<()> {
        let (client_endpoint, server_endpoint) = endpoint();
        let server_addr = server_endpoint.local_addr()?;

        // 4. Server 接收任务
        let server_handle = tokio::spawn(async move {
            println!("Server: Waiting for connection...");
            if let Some(conn) = server_endpoint.accept().await {
                let connection = conn.await.unwrap();
                println!(
                    "Server: Connection accepted from {}",
                    connection.remote_address()
                );

                // 接收双向流
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();

                // 读取数据
                let mut buf = vec![0u8; 10];
                recv.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"ping______");
                println!("Server: Received 'ping______'");

                // 发送回复
                send.write_all(b"pong______").await.unwrap();
                send.finish().unwrap();

                let _ = connection.closed().await;
            }
        });

        // 5. Client 发起连接
        // 注意：这里的 connect 地址必须是 V4，因为你的 try_send 限制了 SocketAddr::V4
        println!("Client: Connecting...");
        let connection = client_endpoint.connect(server_addr, "localhost")?.await?;
        println!("Client: Connected!");

        // 打开流并发送数据
        let (mut send, mut recv) = connection.open_bi().await?;
        send.write_all(b"ping______").await?;
        send.finish()?;

        // 读取回复
        let mut buf = vec![0u8; 10];
        recv.read_exact(&mut buf).await?;
        assert_eq!(&buf, b"pong______");
        println!("Client: Received 'pong______'");

        // 6. 清理
        connection.close(0u32.into(), b"done");
        // 等待 Server 结束
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bandwidth() -> anyhow::Result<()> {
        // --- 3. 定义测试数据量 ---
        // 测试总量: 512 MB
        const TOTAL_SIZE: usize = 32768 * 1024 * 1024;
        // 每次写入块大小: 1 MB (模拟大块写入)
        const CHUNK_SIZE: usize = 1024 * 1024;

        let (client_endpoint, server_endpoint) = endpoint();
        let server_addr = server_endpoint.local_addr()?;

        // --- 4. Server 端 (接收并计时) ---
        let server_handle = tokio::spawn(async move {
            if let Some(conn) = server_endpoint.accept().await {
                let connection = conn.await.unwrap();
                // 接收单向流
                let mut recv = connection.accept_uni().await.unwrap();

                let start = std::time::Instant::now();
                let mut received = 0;

                // 循环读取直到流结束
                // read_chunk 比 read_exact 性能稍好，因为它减少了内部 buffer拷贝
                while let Some(chunk) = recv.read_chunk(usize::MAX, true).await.unwrap() {
                    received += chunk.bytes.len();
                }

                let duration = start.elapsed();
                assert_eq!(received, TOTAL_SIZE, "Data length mismatch");

                let seconds = duration.as_secs_f64();
                let mbps = (received as f64 * 8.0) / (1_000_000.0 * seconds);
                let gbps = mbps / 1000.0;

                println!("--------------------------------------------------");
                println!("Server Recv Statistics:");
                println!("  Total Data: {} MB", received / 1024 / 1024);
                println!("  Duration  : {:.2?}", duration);
                println!("  Throughput: {:.2} Gbps ({:.2} Mbps)", gbps, mbps);
                println!("--------------------------------------------------");

                // 保持连接直到 Client 断开
                let _ = connection.closed().await;
            }
        });

        // --- 5. Client 端 (发送) ---
        let connection = client_endpoint.connect(server_addr, "localhost")?.await?;
        let mut send = connection.open_uni().await?;

        // 构造一个 1MB 的数据块
        let data_chunk = vec![0u8; CHUNK_SIZE];
        let bytes_data = bytes::Bytes::from(data_chunk); // 使用 Bytes 避免重复分配

        println!("Client: Start sending {} MB...", TOTAL_SIZE / 1024 / 1024);
        let start_send = std::time::Instant::now();

        let chunks = TOTAL_SIZE / CHUNK_SIZE;
        for _ in 0..chunks {
            // write_chunk 配合 Bytes 使用效率最高
            send.write_chunk(bytes_data.clone()).await?;
        }

        // 告诉对端发送完毕
        send.finish()?;
        // 等待流彻底关闭（确保对方收到了 FIN）
        send.stopped().await?;

        let send_duration = start_send.elapsed();
        println!("Client: Send finished in {:.2?}", send_duration);

        // 关闭连接
        connection.close(0u32.into(), b"done");

        // 等待 Server 打印结果
        let _ = tokio::time::timeout(Duration::from_secs(5), server_handle).await;

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bandwidth_parallel() -> anyhow::Result<()> {
        // --- 1. 配置参数 ---
        const STREAM_COUNT: usize = 16; // 并发流数量
        const STREAM_SIZE: usize = 1024 * 1024 * 1024; // 每个流发送 256KB

        let (client_endpoint, server_endpoint) = endpoint();
        let server_addr = server_endpoint.local_addr()?;

        // --- 3. Server 端 (并发接收器) ---
        let server_handle = tokio::spawn(async move {
            if let Some(conn) = server_endpoint.accept().await {
                let connection = conn.await.unwrap();
                println!("Server: Accepted connection");

                let mut stream_handles = Vec::new();
                let start = std::time::Instant::now();

                // 接收预期数量的流
                for i in 0..STREAM_COUNT {
                    match connection.accept_uni().await {
                        Ok(mut recv) => {
                            // 为每个流启动一个独立的处理任务
                            let handle = tokio::spawn(async move {
                                // 读取所有数据
                                match recv.read_to_end(usize::MAX).await {
                                    Ok(data) => {
                                        // 校验长度
                                        assert_eq!(
                                            data.len(),
                                            STREAM_SIZE,
                                            "Stream {} length mismatch",
                                            i
                                        );
                                        // 校验数据内容 (验证数据隔离性)
                                        // 我们约定数据的第一个字节是 (stream_index % 255)
                                        // 这样可以确保流的数据没串
                                        let expected_byte = (data[0]) as usize; // 获取实际收到的标记
                                        // 这里只是简单校验首尾，实际生产可以使用 CRC
                                        if data[data.len() - 1] != data[0] {
                                            panic!("Stream data corruption");
                                        }
                                        expected_byte // 返回标记用于统计
                                    }
                                    Err(e) => panic!("Stream read error: {}", e),
                                }
                            });
                            stream_handles.push(handle);
                        }
                        Err(e) => panic!("Failed to accept stream {}: {}", i, e),
                    }
                }

                // 等待所有流处理完毕
                let results = futures::future::join_all(stream_handles).await;
                let duration = start.elapsed();

                let speed = ((STREAM_COUNT * STREAM_SIZE) as f64 * 8.0)
                    / (duration.as_secs_f64() * 1_000_000.0);

                println!("--------------------------------------------------");
                println!("Server: All {} streams received processing.", results.len());
                println!("Total Time: {:.2?}", duration);
                println!(
                    "Total Data: {} MB",
                    (STREAM_COUNT * STREAM_SIZE) / 1024 / 1024
                );
                println!(
                    "Average Speed: {:.2} Gbps ({:.2} Mbps)",
                    speed / 1024.0,
                    speed
                );
                println!("--------------------------------------------------");

                // 保持连接直到 Client 断开
                let _ = connection.closed().await;
            }
        });

        // --- 4. Client 端 (并发发送器) ---
        let connection = client_endpoint.connect(server_addr, "localhost")?.await?;
        println!(
            "Client: Connected, starting {} parallel streams...",
            STREAM_COUNT
        );

        let start_send = std::time::Instant::now();
        let mut client_tasks = Vec::new();

        // 并发启动发送任务
        for i in 0..STREAM_COUNT {
            let conn = connection.clone();
            client_tasks.push(tokio::spawn(async move {
                // 打开单向流
                let mut send = conn.open_uni().await.expect("Failed to open stream");

                // 构造数据：为了验证隔离性，我们用 i 作为填充标记
                // 所有的字节都填成 (i % 255)
                let fill_byte = (i % 255) as u8;
                let data = vec![fill_byte; STREAM_SIZE];
                let bytes_data = bytes::Bytes::from(data);

                send.write_chunk(bytes_data).await.expect("Write failed");
                send.finish().expect("Finish failed");
                // 等待 Server 确认收到 FIN
                send.stopped().await.expect("Stopped failed");
            }));
        }

        // 等待所有发送任务完成
        futures::future::join_all(client_tasks).await;

        let send_duration = start_send.elapsed();
        println!("Client: All streams sent in {:.2?}", send_duration);

        // 关闭连接
        connection.close(0u32.into(), b"done");

        // 等待 Server 结束
        let _ = tokio::time::timeout(Duration::from_secs(10), server_handle).await;

        Ok(())
    }
}
//endregion
