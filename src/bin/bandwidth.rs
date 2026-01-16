use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use quinn::{Endpoint, VarInt};
use std::{net::SocketAddr, time::{Duration, Instant}};
use quinn_plaintext::{client_config, server_config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use qs::transport_config;
// 补充：write_all 需要这个 trait

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
        #[arg(short, long, default_value_t = 5)]
        streams: usize,

        /// 发送的总数据量 (字节)
        #[arg(long, default_value_t = 32)]
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
        } => run_client(streams, total_size * 1048576, addr).await?,
    }

    Ok(())
}

// --- 服务端逻辑 (保持不变) ---

async fn run_server() -> Result<()> {
    let mut server_config = server_config();
    server_config.transport_config(transport_config());

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
                            Ok(Some(_)) => {}
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                });
            }
            Err(e) => {
                println!("Connection closed: {}", e);
                break;
            }
        }
    }
    Ok(())
}

// --- 客户端逻辑 (已修改) ---

async fn run_client(streams_count: usize, total_size: usize, server_addr: SocketAddr) -> Result<()> {
    let mut client_config = client_config();
    client_config.transport_config(transport_config());

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap())?;
    endpoint.set_default_client_config(client_config);

    println!("Connecting to {}...", server_addr);

    let connection = endpoint
        .connect(server_addr, "localhost")?
        .await
        .context("Failed to connect")?;

    println!("Connected! Starting {} streams, total size: {} bytes", streams_count, total_size);

    // 1. 开始计时
    let start_time = Instant::now();

    let bytes_per_stream = if streams_count > 0 { total_size / streams_count } else { 0 };

    let m = MultiProgress::new();

    // 2. 修改样式：增加 {binary_bytes_per_sec} 显示带宽
    // 格式解释: [耗时] 进度条 字节/总字节 (速度, 预计剩余时间) 消息
    let style = ProgressStyle::with_template(
        "[{elapsed_precise}] {bar:40.cyan/blue} {bytes}/{total_bytes} ({binary_bytes_per_sec}, {eta}) {msg}",
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
            loop {
                match perform_stream_task(&conn_clone, bytes_per_stream, &pb).await {
                    Ok(_) => {
                        pb.finish_with_message(format!("Stream #{} Done", i));
                        break;
                    }
                    Err(e) => {
                        pb.set_message(format!("Retry in 3s ({})", e));
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        pb.reset();
                        pb.set_message(format!("Retrying Stream #{}", i));
                    }
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }

    // 3. 删除 m.clear()，保留进度条在屏幕上
    // let _ = m.clear(); <--- 已删除

    // 4. 计算并显示总统计信息
    let duration = start_time.elapsed();
    let total_mb = total_size as f64 / 1024.0 / 1024.0;
    let duration_secs = duration.as_secs_f64();
    let throughput_mb_s = total_mb / duration_secs;

    println!("\n================ SUMMARY ================");
    println!("Total Streams : {}", streams_count);
    println!("Total Data    : {:.2} MiB", total_mb);
    println!("Time Elapsed  : {:.2?}", duration);
    println!("Avg Throughput: {:.2} MiB/s", throughput_mb_s);
    println!("=========================================");

    connection.close(VarInt::from_u32(0), b"done");
    endpoint.wait_idle().await;

    Ok(())
}

// --- 封装单个流的发送任务 (保持你要求的原样) ---
async fn perform_stream_task(
    connection: &quinn::Connection,
    total_bytes: usize,
    pb: &ProgressBar
) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;

    let chunk_size = 64 * 1024;
    let mut remaining = total_bytes;
    let data = vec![0u8; chunk_size];

    while remaining > 0 {
        let to_send = std::cmp::min(remaining, chunk_size);
        send.write_all(&data[..to_send]).await?;
        remaining -= to_send;
        pb.inc(to_send as u64);
    }

    // 保持你要求的写法
    send.finish();
    recv.read_to_end(1048576).await?;

    Ok(())
}