use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use bytes::Bytes;
use quinn::{ClientConfig, Endpoint, ServerConfig, TransportConfig, VarInt};
use quinn_plaintext::{client_config, server_config};
use quinn_proto::congestion::BbrConfig;
use tokio::io::AsyncReadExt;

// 测试参数
const PAYLOAD_SIZE: usize = 1024 * 1024 * 1024; // 1GB
const CHUNK_SIZE: usize = 1024 * 1024;          // 1MB (你的应用层 buffer)

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 开启日志以便排查（可选）
    // tracing_subscriber::fmt::init();

    let server_addr = "127.0.0.1:5000".parse().unwrap();
    let client_addr = "127.0.0.1:0".parse().unwrap();

    println!("=== 官方 Quinn 库基准测试 (Localhost UDP) ===");
    println!("配置: Plaintext, BBR, 15MB Window, Jumbo MTU");

    // 1. 创建 Server
    let (server_endpoint, _server_cert) = make_server_endpoint(server_addr)?;

    // 2. 创建 Client
    let client_endpoint = make_client_endpoint(client_addr)?;

    // 3. 启动 Server 接收任务
    let server_task = tokio::spawn(async move {
        // 接受连接
        if let Some(conn) = server_endpoint.accept().await {
            let connection = conn.await.unwrap();
            // 接受双向流
            if let Ok(mut stream) = connection.accept_bi().await {
                let start = Instant::now();
                let mut received = 0;
                let mut buf = vec![0u8; CHUNK_SIZE]; // 1MB Buffer

                loop {
                    // Quinn 的 read_exact 或 read_to_end 内部处理了分片
                    // 这里我们模拟 read loop
                    match stream.1.read(&mut buf).await {
                        Ok(Some(n)) => {
                            received += n;
                        }
                        Ok(None) => break, // EOF
                        Err(e) => panic!("Server read error: {}", e),
                    }
                }
                let duration = start.elapsed();
                return (received, duration);
            }
        }
        (0, Duration::from_secs(0))
    });

    // 等待 Server 就绪
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Client 连接并发送
    let connection = client_endpoint.connect(server_addr, "localhost")?.await?;
    let (mut send_stream, mut recv_stream) = connection.open_bi().await?;

    let data = vec![0u8; CHUNK_SIZE]; // 1MB Chunk
    let mut sent = 0;

    // 预先转为 Bytes 以避免循环内分配
    let bytes_data = Bytes::from(data);

    let start = Instant::now();
    while sent < PAYLOAD_SIZE {
        // write_all 会处理发送缓冲
        send_stream.write_all(&bytes_data).await?;
        sent += CHUNK_SIZE;
    }
    // 关闭流发送 FIN
    send_stream.finish()?;

    // 等待 Server 确认所有数据接收完毕（可选，但推荐）
    // 也可以读取 Server 的响应，这里为了纯测吞吐，只需确保 Server 读完
    let _ = recv_stream.read_to_end(10).await;

    // 5. 获取结果
    let (bytes, duration) = server_task.await?;

    let mb = bytes as f64 / 1024.0 / 1024.0;
    let secs = duration.as_secs_f64();
    println!("\n结果:");
    println!("传输数据: {:.2} MB", mb);
    println!("总耗时:   {:.4} s", secs);
    println!("平均速度: {:.2} MB/s ({:.2} Gbps)", mb / secs, (mb * 8.0) / 1024.0 / secs);

    Ok(())
}

// --- 配置辅助函数 ---

fn configure_transport() -> TransportConfig {
    let mut config = TransportConfig::default();

    // 1. 窗口大小 (与你的 quic2 保持一致)
    config.stream_receive_window(VarInt::from_u32(10 * 1024 * 1024));
    config.receive_window(VarInt::from_u32(15 * 1024 * 1024));
    config.send_window(15 * 1024 * 1024); // 显式设置发送窗口

    // 2. 拥塞控制 (BBR)
    config.congestion_controller_factory(Arc::new(BbrConfig::default()));

    // 3. MTU 优化 (Jumbo Frames)
    // 告诉 Quinn 我们希望用大包。Localhost 支持 64k 包。
    config.initial_mtu(65_535);
    config.min_mtu(65_535);
    // 开启 GSO (Generic Segmentation Offload) 支持
    config.enable_segmentation_offload(true);
    // 禁用 MTU 探测，直接用大的
    config.mtu_discovery_config(None);

    config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
    config.max_idle_timeout(Some(VarInt::from_u32(30_000).into()));

    config
}

fn make_server_endpoint(bind_addr: SocketAddr) -> anyhow::Result<(Endpoint, Vec<u8>)> {
    // 使用 plaintext server config
    let mut server_conf = server_config();
    let transport = configure_transport();
    server_conf.transport = Arc::new(transport);

    let endpoint = Endpoint::server(server_conf, bind_addr)?;
    Ok((endpoint, vec![]))
}

fn make_client_endpoint(bind_addr: SocketAddr) -> anyhow::Result<Endpoint> {
    // 使用 plaintext client config
    let mut client_conf = client_config();
    let transport = configure_transport();
    client_conf.transport_config(Arc::new(transport));

    // 绑定到 0 端口
    let mut endpoint = Endpoint::client(bind_addr)?;
    endpoint.set_default_client_config(client_conf);
    Ok(endpoint)
}