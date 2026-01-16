use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use quinn::{ClientConfig, Endpoint, ServerConfig, VarInt};
use std::{net::SocketAddr, sync::Arc, time::Instant};
use quinn_plaintext::{client_config, server_config};
use tokio::io::AsyncReadExt;

// CLI 参数定义
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 运行服务端模式
    Server,
    /// 运行客户端模式
    Client {
        /// 并发流的数量
        #[arg(short, long, default_value_t = 10)]
        streams: usize,

        /// 发送的总数据量 (字节)，例如 10485760 代表 10MB
        #[arg(long, default_value_t = 10 * 1024 * 1024)]
        total_size: usize,

        /// 服务器地址
        #[arg(long, default_value = "127.0.0.1:5000")]
        addr: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server => run_server().await?,
        Commands::Client {
            streams,
            total_size,
            addr,
        } => run_client(streams, total_size, addr).await?,
    }

    Ok(())
}

// --- 服务端逻辑 ---

async fn run_server() -> Result<()> {
    // 1. 生成自签名证书 (QUIC 强制要求 TLS)
    let server_config = server_config();

    // 2. 绑定 UDP 端口 5000
    let addr = "0.0.0.0:5000".parse::<SocketAddr>()?;
    let endpoint = Endpoint::server(server_config, addr)?;

    println!("Server listening on {}", endpoint.local_addr()?);

    // 3. 接受连接
    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            match incoming.await {
                Ok(connection) => {
                    println!("New connection from: {}", connection.remote_address());
                    // 处理该连接中的所有流
                    if let Err(e) = handle_connection(connection).await {
                        eprintln!("Connection error: {}", e);
                    }
                }
                Err(e) => eprintln!("Handshake failed: {}", e),
            }
        });
    }

    Ok(())
}

async fn handle_connection(connection: quinn::Connection) -> Result<()> {
    let start = Instant::now();
    let mut total_bytes_received = 0;

    // 循环接受新的双向流 (Bi-directional stream)
    loop {
        // accept_bi() 返回 (SendStream, RecvStream)
        // 这里的 match 用于处理连接关闭的情况
        match connection.accept_bi().await {
            Ok((_send, mut recv)) => {
                tokio::spawn(async move {
                    // 读取流中的数据直到结束
                    let mut buf = [0u8; 64 * 1024]; // 64KB buffer
                    loop {
                        match recv.read(&mut buf).await {
                            Ok(Some(_n)) => {
                                // 实际应用中你可能会处理数据，这里我们只是为了消耗流量
                            }
                            Ok(None) => break, //流结束
                            Err(_) => break,
                        }
                    }
                });
                total_bytes_received += 1; // 简单计数，实际统计需要更复杂的原子计数器
            }
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                println!("Connection closed by client.");
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }
    println!("Connection finished. Duration: {:?}", start.elapsed());
    println!("Total bytes received: {}", total_bytes_received);
    Ok(())
}

// --- 客户端逻辑 ---

async fn run_client(streams_count: usize, total_size: usize, server_addr: SocketAddr) -> Result<()> {
    // 1. 配置客户端 (跳过证书验证以便测试)
    let client_config = client_config();

    // 2. 绑定本地任意端口
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())?;
    endpoint.set_default_client_config(client_config);

    // 3. 连接服务器
    println!("Connecting to {}...", server_addr);
    let connection = endpoint
        .connect(server_addr, "localhost")?
        .await
        .context("Failed to connect")?;

    println!("Connected! Starting benchmark...");
    println!("Total Size: {} bytes, Streams: {}", total_size, streams_count);

    let start = Instant::now();

    // 计算每个流需要发送的数据量
    let bytes_per_stream = if streams_count > 0 { total_size / streams_count } else { 0 };
    let mut handles = Vec::new();

    // 4. 并发开启流并发送数据
    for i in 0..streams_count {
        let conn_clone = connection.clone();

        let handle = tokio::spawn(async move {
            // 打开双向流
            let (mut send, mut recv) = conn_clone.open_bi().await.expect("Failed to open stream");

            // 构造一些垃圾数据发送 (分块发送以避免内存爆炸)
            let chunk_size = 1024 * 64; // 64KB chunks
            let mut remaining = bytes_per_stream;
            let data = vec![0u8; chunk_size]; // 全0数据用于测试

            while remaining > 0 {
                let to_send = std::cmp::min(remaining, chunk_size);
                // 写入数据
                send.write_all(&data[..to_send]).await.expect("Failed to write");
                remaining -= to_send;
            }

            // 重要：发送 FIN 标志，告诉服务器数据发完了
            send.finish().expect("Failed to finish stream");

            // 等待服务器确认流关闭（可选，取决于是否需要读取服务器回传）
            // 在 Bi 流中，我们通常也要处理接收端，这里简单读到结束
            let _ = recv.read_to_end(1024).await;

            if i % 10 == 0 {
                // 简单的进度打印
                // print!(".");
            }
        });
        handles.push(handle);
    }

    // 等待所有流完成
    for handle in handles {
        handle.await?;
    }

    let duration = start.elapsed();
    let throughput = (total_size as f64 / 1024.0 / 1024.0) / duration.as_secs_f64();

    println!("\nDone!");
    println!("Time elapsed: {:?}", duration);
    println!("Throughput: {:.2} MB/s", throughput);

    // 关闭连接
    connection.close(VarInt::from_u32(0), b"done");
    // 等待后台任务清理
    endpoint.wait_idle().await;

    Ok(())
}
