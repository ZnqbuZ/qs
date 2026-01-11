mod gateway;

// 假设你的库名为 my_quic_lib，且相关模块是公开的
// 如果是在同一个 crate 内部测试，使用 crate::gateway::quic...
#[allow(unused_imports)]
use crate::gateway::quic::{QuicEndpoint, QuicOutputRx, QuicPacket, QuicPacketMargins, QuicStream};
use bytes::{Bytes, BytesMut};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, trace};
use tracing_subscriber::EnvFilter;

// 模拟的客户端和服务器地址
const SERVER_ADDR: &str = "127.0.0.1:4433";
const CLIENT_ADDR: &str = "127.0.0.1:10000";

const TEST1: bool = false;
const PAYLOAD_SIZE_1: usize = 8192 * 1024 * 1024;

const TEST2: bool = true;
const ITERATION_COUNT: usize = 100_000;

const TEST3: bool = true;
const STREAM_COUNT: usize = 4;
const PAYLOAD_SIZE_3: usize = 4096 * 1024 * 1024;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // console_subscriber::init();

    let filter = if cfg!(debug_assertions) {
        // Debug 构建，打印所有 debug / trace
        EnvFilter::new("qs=trace")
    } else {
        // Release 构建，只打印 info 以上
        EnvFilter::new("qs=info")
    };

    // 开启日志以便观察握手过程（可选）
    tracing_subscriber::fmt().with_env_filter(filter).init();

    info!("=== 开始 QUIC 库性能基准测试 ===");
    info!("测试环境: 内存直连 (In-memory), 排除 OS UDP 栈干扰");

    if TEST1 { benchmark_throughput().await; }
    if TEST2 { benchmark_latency_pps().await;}
    if TEST3 { benchmark_concurrent_throughput().await;}
}

/// 测试 1: 最大单流吞吐量 (Bandwidth)
async fn benchmark_throughput() {
    trace!("--- 测试 1: 单流吞吐量 (1GB 数据传输) ---");

    let server_addr: SocketAddr = SERVER_ADDR.parse().unwrap();
    let margins = QuicPacketMargins {
        header: 0,
        trailer: 0,
    };

    // 2. 启动虚拟网络

    // 3. Server 端接受流的处理逻辑

    // 为了测试方便，我们需要把 stream channel 分离出来。
    // 由于 QuicOutputRx 的字段是公有的，我们可以解构它。
    let (server, server_out) = QuicEndpoint::new(margins);
    let (client, client_out) = QuicEndpoint::new(margins);

    let server_packet_rx = server_out.packet;
    let mut server_new_streams = server_out.stream;

    let client_packet_rx = client_out.packet;
    let _client_new_streams = client_out.stream; // Client 端不需要 accept 流，它是发起方

    let server = Arc::new(server);
    let client = Arc::new(client);

    // 启动网络转发（只转发 Packet）
    let s_arc = server.clone();
    let c_arc = client.clone();

    // --- [修复 1] Network: Client -> Server ---
    // 这个任务压力最大，必须 yield，否则会饿死 ACK 回传任务
    tokio::spawn(async move {
        let mut rx = client_packet_rx;
        // 使用 limit 限制每次连续处理的包数量，避免单次占用时间过长
        let mut count = 0;
        while let Some(pkt) = rx.recv().await {
            // 如果 send 返回 Err，说明 Server 已经关闭/崩溃，我们应该退出而不是 Panic
            if s_arc
                .send(CLIENT_ADDR.parse().unwrap(), pkt.payload)
                .await
                .is_err()
            {
                break;
            }

            // 每转发 16 个包，强制让出一次 CPU
            // 这给了 Server 处理包和生成 ACK 的机会，也给了另一个网络任务转发 ACK 的机会
            count += 1;
            if count >= 16 {
                count = 0;
                tokio::task::yield_now().await;
            }
        }
    });

    // --- [修复 2] Network: Server -> Client ---
    // 负责转发 ACK
    tokio::spawn(async move {
        let mut rx = server_packet_rx;
        let mut count = 0;
        while let Some(pkt) = rx.recv().await {
            if c_arc
                .send(SERVER_ADDR.parse().unwrap(), pkt.payload)
                .await
                .is_err()
            {
                break;
            }
            // 同样加入 yield
            count += 1;
            if count >= 16 {
                count = 0;
                tokio::task::yield_now().await;
            }
        }
    });

    // 4. Server 端：开启一个任务接收数据并丢弃 (Sink)
    let server_handle = tokio::spawn(async move {
        trace!("Server: 等待接收数据...");
        if let Some(mut stream) = server_new_streams.recv().await {
            let mut buf = vec![0u8; 64 * 1024]; // 64KB buffer
            let mut total_bytes = 0;
            let start = Instant::now();
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                trace!("Server: 接收数据... 已接收 {} bytes", total_bytes + n);
                if n == 0 {
                    break;
                }
                total_bytes += n;
            }
            trace!("Server: 数据接收完毕，总计 {} bytes", total_bytes);
            let duration = start.elapsed();
            return (total_bytes, duration);
        }
        (0, Duration::from_secs(0))
    });

    // 5. Client 端：发起连接并狂发数据
    // 等待一下让网络 setup
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client_stream_task = tokio::spawn(async move {
        // 等待握手完成并获取流
        let mut stream = loop {
            // Scope 1: 临界区，持有锁
            {
                trace!("Client: 尝试打开流...");
                let endpoint = client.clone();
                // 尝试打开流
                if let Ok(s) = endpoint.open(server_addr, None).await {
                    break s; // 成功！跳出循环，锁会自动释放
                }
                // 如果失败，锁会在这个花括号结束时自动释放
                // 或者你可以手动调用 drop(endpoint);
            }

            // Scope 2: 无锁状态
            // 关键：此时没有持有锁！网络转发任务可以获取锁并填入 Server 的响应包
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        trace!("Client: 流已打开，开始发送数据...");
        // 拿到流之后继续业务逻辑...
        let payload_size = PAYLOAD_SIZE_1; // 1GB
        let chunk_size = 64 * 1024;
        let data = vec![1u8; chunk_size];

        let mut sent = 0;
        while sent < payload_size {
            trace!("Client: 发送数据... {}/{}", sent, payload_size);
            stream.write_all(&data).await.unwrap();
            sent += chunk_size;
        }
        trace!("Client: 数据发送完毕，总计 {} bytes", sent);
        stream.shutdown().await.unwrap();
    });

    client_stream_task.await.unwrap();
    let (bytes, duration) = server_handle.await.unwrap();

    let mb = bytes as f64 / 1024.0 / 1024.0;
    let secs = duration.as_secs_f64();
    println!("传输: {:.2} MB, 耗时: {:.4} s", mb, secs);
    println!(
        "速度: {:.2} MB/s ({:.2} Gbps)",
        mb / secs,
        (mb * 8.0) / 1024.0 / secs
    );
}

/// 测试 2: 延迟与 PPS (Ping-Pong)
async fn benchmark_latency_pps() {
    println!("\n--- 测试 2: 往返延迟 (Latency) & PPS ---");
    let margins = QuicPacketMargins {
        header: 0,
        trailer: 0,
    };
    let (server, server_out) = QuicEndpoint::new(margins);
    let (client, client_out) = QuicEndpoint::new(margins);

    let server_packet_rx = server_out.packet;
    let mut server_new_streams = server_out.stream;
    let client_packet_rx = client_out.packet;

    let server = Arc::new(server);
    let client = Arc::new(client);

    // Wiring
    let s_arc = server.clone();
    let c_arc = client.clone();
    tokio::spawn(async move {
        let mut rx = client_packet_rx;
        while let Some(pkt) = rx.recv().await {
            trace!("Network: Client -> Server packet");
            s_arc
                .send(CLIENT_ADDR.parse().unwrap(), pkt.payload)
                .await
                .unwrap();
        }
    });
    tokio::spawn(async move {
        let mut rx = server_packet_rx;
        while let Some(pkt) = rx.recv().await {
            trace!("Network: Server -> Client packet");
            c_arc
                .send(SERVER_ADDR.parse().unwrap(), pkt.payload)
                .await
                .unwrap();
        }
    });

    // Server: Echo Server
    tokio::spawn(async move {
        while let Some(mut stream) = server_new_streams.recv().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Client: Ping Pong
    let endpoint = client.clone();
    let mut stream = loop {
        // Scope 1: 临界区，持有锁
        {
            trace!("Client: 尝试打开流...");
            let endpoint = client.clone();
            // 尝试打开流
            if let Ok(s) = endpoint.open(SERVER_ADDR.parse().unwrap(), None).await {
                break s; // 成功！跳出循环，锁会自动释放
            }
            // 如果失败，锁会在这个花括号结束时自动释放
            // 或者你可以手动调用 drop(endpoint);
        }

        // Scope 2: 无锁状态
        // 关键：此时没有持有锁！网络转发任务可以获取锁并填入 Server 的响应包
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    drop(endpoint);

    let payload = vec![0u8; 64]; // 小包 64字节
    let mut buf = vec![0u8; 1024];
    let iterations = ITERATION_COUNT;

    let start = Instant::now();
    for _ in 0..iterations {
        stream.write_all(&payload).await.unwrap();
        stream.read_exact(&mut buf[..64]).await.unwrap();
    }
    let duration = start.elapsed();

    let avg_latency = duration.as_secs_f64() * 1_000_000.0 / iterations as f64;
    let pps = iterations as f64 / duration.as_secs_f64();

    println!("Iterations: {}", iterations);
    println!("平均 RTT 延迟: {:.2} µs", avg_latency);
    println!("PPS (Transactions/s): {:.2}", pps);
}

/// 测试 3: 多流并发吞吐量 (Concurrent Throughput)
async fn benchmark_concurrent_throughput() {
    info!("\n--- 测试 3: 多流并发吞吐量 (Concurrent Throughput) ---");
    // 参数配置
    let stream_count = STREAM_COUNT; // 并发流数量
    let size_per_stream = PAYLOAD_SIZE_3; // 每个流发送 10MB
    let total_expected = stream_count as u64 * size_per_stream as u64;

    let margins = QuicPacketMargins {
        header: 0,
        trailer: 0,
    };
    // 创建新的端点实例，环境是隔离的
    let (server, server_out) = QuicEndpoint::new(margins);
    let (client, client_out) = QuicEndpoint::new(margins);

    let server_packet_rx = server_out.packet;
    let mut server_new_streams = server_out.stream;
    let client_packet_rx = client_out.packet;

    // Client 不需要 accept 流，所以这里忽略 client_out.stream

    let server = Arc::new(server);
    let client = Arc::new(client);

    // 启动虚拟网络转发 (Network Simulation)
    let s_arc = server.clone();
    let c_arc = client.clone();

    // Client -> Server
    tokio::spawn(async move {
        let mut rx = client_packet_rx;
        // 使用 limit 限制每次连续处理的包数量，避免单次占用时间过长
        let mut count = 0;
        while let Some(pkt) = rx.recv().await {
            // 如果 send 返回 Err，说明 Server 已经关闭/崩溃，我们应该退出而不是 Panic
            if s_arc
                .send(CLIENT_ADDR.parse().unwrap(), pkt.payload)
                .await
                .is_err()
            {
                break;
            }

            // 每转发 16 个包，强制让出一次 CPU
            // 这给了 Server 处理包和生成 ACK 的机会，也给了另一个网络任务转发 ACK 的机会
            count += 1;
            if count >= 16 {
                count = 0;
                tokio::task::yield_now().await;
            }
        }
    });

    // Server -> Client
    tokio::spawn(async move {
        let mut rx = server_packet_rx;
        let mut count = 0;
        while let Some(pkt) = rx.recv().await {
            if c_arc
                .send(SERVER_ADDR.parse().unwrap(), pkt.payload)
                .await
                .is_err()
            {
                break;
            }
            // 同样加入 yield
            count += 1;
            if count >= 16 {
                count = 0;
                tokio::task::yield_now().await;
            }
        }
    });

    // --- Server 端逻辑：并发接收 ---
    let server_handle = tokio::spawn(async move {
        let mut join_set = tokio::task::JoinSet::new();
        let mut total_bytes_received = 0;

        // 我们预期接收 stream_count 个流
        let mut accepted_count = 0;
        while let Some(mut stream) = server_new_streams.recv().await {
            join_set.spawn(async move {
                let mut buf = vec![0u8; 64 * 1024]; // 64KB buffer
                let mut stream_received = 0;
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) => break, // EOF
                        Ok(n) => stream_received += n,
                        Err(_) => break, // Error or Reset
                    }
                }
                stream_received
            });
            accepted_count += 1;
            if accepted_count > stream_count {
                break;
            }
        }

        // 等待所有流接收完毕并统计流量
        while let Some(res) = join_set.join_next().await {
            total_bytes_received += res.unwrap_or(0);
        }
        total_bytes_received
    });

    // --- Client 端逻辑：握手并并发发送 ---
    let client_handle = tokio::spawn(async move {
        // 1. 等待握手完成 (Wait for handshake)
        // 这里的逻辑与之前修复的一样，通过不断尝试 open 来确保连接建立
        let endpoint = client.clone();
        loop {
            match endpoint.open(SERVER_ADDR.parse().unwrap(), None).await {
                Ok(mut s) => {
                    // 握手成功，关闭这个探测流
                    let _ = s.shutdown().await;
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        trace!("Client: 握手完成，开始并发发送...");

        let start = Instant::now();
        let mut join_set = tokio::task::JoinSet::new();
        // 预分配一个只读数据块，避免测试中频繁分配内存影响结果
        let data = Arc::new(vec![1u8; 64 * 1024]);

        for i in 0..stream_count {
            let ep = client.clone();
            let data = data.clone();
            join_set.spawn(async move {
                let addr = SERVER_ADDR.parse().unwrap();
                // 打开流
                let mut stream = ep.open(addr, None).await.expect("Failed to open stream");

                let mut sent = 0;
                while sent < size_per_stream {
                    // 发送数据
                    stream.write_all(&data).await.expect("Write failed");
                    sent += data.len();
                }
                // 关闭写端，发送 FIN
                info!("Client: Stream {} sent {} bytes, shutting down", i, sent);
                stream.shutdown().await.expect("Shutdown failed");
                trace!("Client: Stream {} finished", i);
            });
        }

        // 等待所有发送任务完成
        while let Some(res) = join_set.join_next().await {
            res.unwrap();
        }

        start.elapsed()
    });

    // 等待测试结束
    let duration = client_handle.await.unwrap();
    let total_bytes = server_handle.await.unwrap();

    // 结果输出
    let mb = total_bytes as f64 / 1024.0 / 1024.0;
    let secs = duration.as_secs_f64();
    let throughput_mb = mb / secs;
    let throughput_gbps = (mb * 8.0) / 1024.0 / secs;

    info!("--- 测试结果 ---");
    info!("并发流数量: {}", stream_count);
    info!(
        "总接收数据: {:.2} MB (预期: {:.2} MB)",
        mb,
        total_expected as f64 / 1024.0 / 1024.0
    );
    info!("总耗时: {:.4} s", secs);
    info!(
        "聚合带宽: {:.2} MB/s ({:.2} Gbps)",
        throughput_mb, throughput_gbps
    );
}
