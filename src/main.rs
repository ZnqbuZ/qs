use clap::builder::styling::Color;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use quinn::{ClientConfig, Endpoint, ServerConfig, VarInt};
use std::{net::SocketAddr, sync::Arc, time::{Duration, Instant}};
use quinn_plaintext::{client_config, server_config};
use tokio::io::AsyncReadExt;

// --- 注意：为了代码可运行，我这里恢复了标准的证书生成逻辑 ---
// 如果你有 quinn_plaintext，请替换回你的 use quinn_plaintext::*;
// 并修改 configure_server 和 configure_client 函数

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Server,
    Client {
        /// 并发流的数量
        #[arg(short, long, default_value_t = 5)] // 默认改小一点以便观察进度条
        streams: usize,

        /// 发送的总数据量 (字节)
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
    let server_config = server_config(); // 使用下方定义的辅助函数
    let addr = "0.0.0.0:5000".parse::<SocketAddr>()?;
    let endpoint = Endpoint::server(server_config, addr)?;

    println!("Server listening on {}", endpoint.local_addr()?);

    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            match incoming.await {
                Ok(connection) => {
                    println!("New connection: {}", connection.remote_address());
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
    loop {
        match connection.accept_bi().await {
            Ok((_send, mut recv)) => {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64 * 1024];
                    loop {
                        match recv.read(&mut buf).await {
                            Ok(Some(_)) => {} // 接收数据，不做处理
                            Ok(None) => break, // 流结束
                            Err(_) => break,   // 出错
                        }
                    }
                });
            }
            Err(e) => {
                // 连接断开
                println!("Connection closed: {}", e);
                break;
            }
        }
    }
    Ok(())
}

// --- 客户端逻辑 ---

async fn run_client(streams_count: usize, total_size: usize, server_addr: SocketAddr) -> Result<()> {
    let client_config = client_config();

    // 绑定端口
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())?;
    endpoint.set_default_client_config(client_config);

    println!("Connecting to {}...", server_addr);

    // 建立主连接
    let connection = endpoint
        .connect(server_addr, "localhost")?
        .await
        .context("Failed to connect")?;

    println!("Connected! Starting {} streams, total size: {} bytes", streams_count, total_size);

    let bytes_per_stream = if streams_count > 0 { total_size / streams_count } else { 0 };

    // 初始化多进度条管理器
    let m = MultiProgress::new();
    let style = ProgressStyle::with_template(
        "[{elapsed_precise}] {bar:40.cyan/blue} {bytes}/{total_bytes} {msg}",
    )
        .unwrap()
        .progress_chars("##-");

    let mut handles = Vec::new();

    for i in 0..streams_count {
        let conn_clone = connection.clone();
        let pb = m.add(ProgressBar::new(bytes_per_stream as u64));
        pb.set_style(style.clone());
        pb.set_message(format!("Stream #{}", i));

        let handle = tokio::spawn(async move {
            // 这里是重试循环：如果 perform_stream_task 失败，则无限重试
            loop {
                match perform_stream_task(&conn_clone, bytes_per_stream, &pb).await {
                    Ok(_) => {
                        pb.finish_with_message(format!("Stream #{} Done", i));
                        break; // 成功完成，跳出循环
                    }
                    Err(e) => {
                        // 失败处理
                        pb.set_message(format!("Retry in 3s ({})", e));
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        pb.reset(); // 重置进度条（因为我们要重新发这个流的所有数据）
                        pb.set_message(format!("Retrying Stream #{}", i));
                    }
                }
            }
        });
        handles.push(handle);
    }

    // 等待所有任务完成
    for handle in handles {
        let _ = handle.await;
    }

    // 清除进度条以便显示最终统计
    let _ = m.clear();

    println!("All streams finished!");
    connection.close(VarInt::from_u32(0), b"done");
    endpoint.wait_idle().await;

    Ok(())
}

// --- 封装单个流的发送任务 ---
async fn perform_stream_task(
    connection: &quinn::Connection,
    total_bytes: usize,
    pb: &ProgressBar
) -> Result<()> {
    // 1. 尝试打开流 (如果连接断了，这里会报错)
    let (mut send, mut recv) = connection.open_bi().await?;

    let chunk_size = 64 * 1024;
    let mut remaining = total_bytes;
    let data = vec![0u8; chunk_size]; // 模拟数据

    // 2. 发送数据循环
    while remaining > 0 {
        let to_send = std::cmp::min(remaining, chunk_size);

        // 写入数据
        send.write_all(&data[..to_send]).await?;

        remaining -= to_send;
        pb.inc(to_send as u64); // 更新进度条
    }

    // 3. 正常关闭流
    send.finish(); // 不用 await

    // 4. 等待服务器确认收到（可选，但在双向流中通常需要读取对方的 FIN 或响应）
    // 如果不需要读数据，只读到 End Of Stream 即可
    recv.read_to_end(1048576).await?;

    Ok(())
}