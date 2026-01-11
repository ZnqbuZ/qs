mod gateway;

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};
use log::error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tracing::{info, trace};
use tracing_subscriber::EnvFilter;
// 假设你的库名为 my_quic_lib
// use my_quic_lib::gateway::quic::{QuicEndpoint, QuicPacketMargins, QuicCtrl, QuicPacket};
// 这里使用你提供的代码中的路径，你需要根据实际情况调整 use 路径

// --- 模拟网络转发层 ---
// 它的作用是把 Server 出去的包塞给 Client 的输入，反之亦然。
// 这样完全跳过了 UDP Socket 系统调用，只测试逻辑性能。
async fn run_virtual_network(
    mut server_pkt_rx: crate::gateway::quic::QuicPacketRx,
    server_ctrl: Arc<crate::gateway::quic::QuicCtrl>,
    mut client_pkt_rx: crate::gateway::quic::QuicPacketRx,
    client_ctrl: Arc<crate::gateway::quic::QuicCtrl>,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
) {
    let s2c = tokio::spawn(async move {
        while let Some(mut pkt) = server_pkt_rx.recv().await {
            // 在真实网络中，IP头会被剥离，这里我们模拟接收端看到的“对端地址”
            // Server 发给 Client，Client 收到时看到的是 Server 的地址
            pkt.addr = server_addr;
            if let Err(e) = client_ctrl.send(pkt).await {
                error!("Virtual network: failed to forward packet from server to client: {}", e);
                break;
            }
        }
    });

    let c2s = tokio::spawn(async move {
        while let Some(mut pkt) = client_pkt_rx.recv().await {
            // Client 发给 Server，Server 收到时看到的是 Client 的地址
            pkt.addr = client_addr;
            if let Err(e) = server_ctrl.send(pkt).await {
                error!("Virtual network: failed to forward packet from client to server: {}", e);
                break;
            }
        }
    });

    // 等待任意一方断开
    let _ = tokio::select! {
        _ = s2c => {},
        _ = c2s => {},
    };
}

// --- 基础环境搭建 ---
struct TestEnv {
    server: crate::gateway::quic::QuicEndpoint,
    client: crate::gateway::quic::QuicEndpoint,
    server_ctrl: Arc<crate::gateway::quic::QuicCtrl>,
    client_ctrl: Arc<crate::gateway::quic::QuicCtrl>,
    server_stream_rx: crate::gateway::quic::QuicStreamRx,
    _network_handle: tokio::task::JoinHandle<()>,
}

async fn setup_env() -> TestEnv {
    let server_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    let client_addr: SocketAddr = "127.0.0.1:6000".parse().unwrap();
    let margins = crate::gateway::quic::QuicPacketMargins { header: 0, trailer: 0 };

    // 1. 启动 Server
    let mut server = crate::gateway::quic::QuicEndpoint::new();
    let (server_pkt_rx, server_stream_rx) = server.run(margins).unwrap();
    let server_ctrl = server.ctrl().unwrap();

    // 2. 启动 Client
    let mut client = crate::gateway::quic::QuicEndpoint::new();
    let (client_pkt_rx, _client_stream_rx) = client.run(margins).unwrap();
    _client_stream_rx.switch().set(false);
    let client_ctrl = client.ctrl().unwrap();

    // 3. 启动虚拟网络
    let s_ctrl_clone = server_ctrl.clone();
    let c_ctrl_clone = client_ctrl.clone();
    let net_handle = tokio::spawn(async move {
        run_virtual_network(server_pkt_rx, s_ctrl_clone, client_pkt_rx, c_ctrl_clone, client_addr, server_addr).await;
    });

    TestEnv {
        server,
        client,
        server_ctrl,
        client_ctrl,
        server_stream_rx,
        _network_handle: net_handle,
    }
}

// --- 测试场景 1: 最大吞吐量 (Bulk Transfer) ---
async fn bench_throughput(total_size_mb: usize) {
    trace!("=== 开始测试: 吞吐量 ({} MB) ===", total_size_mb);
    let mut env = setup_env().await;
    let server_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

    let payload_size = 1024 * 32; // 32KB chunks
    let total_bytes = total_size_mb * 1024 * 1024;
    let chunk_count = total_bytes / payload_size;

    let data_chunk = Bytes::from(vec![0u8; payload_size]);

    // Server 端接收并统计
    let server_task = tokio::spawn(async move {
        // 接收第一个流
        if let Some(mut stream) = env.server_stream_rx.recv().await {
            info!("Received stream from client");
            stream.ready().await.expect("Server stream ready failed");
            let mut buf = vec![0u8; 65535];
            let mut received = 0;
            while let Ok(n) = stream.read(&mut buf).await {
                if n == 0 { break; }
                received += n;
                trace!("Received {} bytes", received);
            }
            return received;
        }
        0
    });

    // Client 端发送
    let start = Instant::now();
    let mut client_stream = match env.client_ctrl.connect(server_addr, None, true).await {
        Ok(s) => s,
        Err(_) => {
            // 连接指令已发出，Driver 正在后台进行 QUIC 握手
            // 稍等片刻让 Transport Parameters 交换完成
            tokio::time::sleep(Duration::from_millis(100)).await;
            // 第二次尝试：此时应该已经 Connected 且有了 Stream Credit
            env.client_ctrl.connect(server_addr, None, true).await.expect("Retry connect failed")
        }
    };

    for _ in 0..chunk_count {
        client_stream.write_all(&data_chunk).await.expect("Write failed");
        trace!("Sent {} bytes", payload_size);
    }
    client_stream.shutdown().await.expect("Shutdown failed"); // 发送 FIN

    let received_bytes = server_task.await.unwrap();
    let duration = start.elapsed();

    let mb_per_sec = (received_bytes as f64 / 1024.0 / 1024.0) / duration.as_secs_f64();
    println!("完成。耗时: {:?}, 接收: {} MB", duration, received_bytes / 1024 / 1024);
    println!("带宽: {:.2} MB/s {:.2} Gbps (注意：这是纯内存拷贝+协议开销)", mb_per_sec, mb_per_sec * 8.0 / 1024.0);
}

// --- 测试场景 2: 小包每秒处理能力 (PPS / Latency) ---
async fn bench_pps(iterations: usize) {
    println!("=== 开始测试: PPS/Ping-Pong ({} 次) ===", iterations);
    let mut env = setup_env().await;
    let server_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

    // Server 端: Echo Server
    tokio::spawn(async move {
        while let Some(mut stream) = env.server_stream_rx.recv().await {
            tokio::spawn(async move {
                stream.ready().await.unwrap();
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            // 收到数据立即回写
                            if let Err(_) = stream.write_all(&buf[..n]).await { break; }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    // Client 端: 发送 Ping 等待 Pong
    let mut client_stream = env.client_ctrl.connect(server_addr, None, true).await.expect("Connect failed");
    let ping_data = Bytes::from_static(b"ping");
    let mut buf = [0u8; 1024];

    let start = Instant::now();
    for _ in 0..iterations {
        client_stream.write_all(&ping_data).await.unwrap();
        // 等待回复（同步阻塞式模拟 ping-pong）
        let _ = client_stream.read(&mut buf).await.unwrap();
    }
    let duration = start.elapsed();

    let pps = iterations as f64 / duration.as_secs_f64();
    let avg_latency = duration.as_secs_f64() * 1000.0 * 1000.0 / iterations as f64; // microseconds

    println!("完成。耗时: {:?}", duration);
    println!("PPS: {:.0} msgs/s", pps);
    println!("平均往返延迟 (RTT): {:.2} us", avg_latency);
}

// --- 测试场景 3: 大量并发连接 (Concurrency) ---
async fn bench_concurrency(conn_count: usize) {
    println!("=== 开始测试: 并发连接数 ({} 连接) ===", conn_count);
    let mut env = setup_env().await;
    let server_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();

    // Server 只要不断接受连接即可
    tokio::spawn(async move {
        let mut count = 0;
        while let Some(mut stream) = env.server_stream_rx.recv().await {
            stream.ready().await.unwrap();
            count += 1;
            if count >= conn_count {
                break; // 只要连接建立成功即可
            }
        }
    });

    let start = Instant::now();
    let mut set = tokio::task::JoinSet::new();

    // 并发发起连接
    // 注意：QuicDriver 是单线程处理所有连接的，这里的并发主要测试 channel 和 driver 的处理能力
    for i in 0..conn_count {
        let ctrl = env.client_ctrl.clone();
        // 模拟不同的源端口，否则 quinn 可能会认为是同一个连接迁移
        // 在我们上面的 virtual_network 里比较简单粗暴，真实情况需要更复杂的模拟
        // 由于我们的 virtual_network 强制修改了 addr，这里其实是在模拟复用同一个 connection 创建 stream
        // 如果要测试 *Connection* 数量，我们需要在 Client 端创建多个 Endpoint，或者修改 virtual_network 支持根据 stream ID 路由（这比较复杂）。
        // 鉴于 quic/driver.rs 的实现，我们这里测试的是 *Stream* 的并发开启能力更为直接。

        set.spawn(async move {
            let _ = ctrl.connect(server_addr, Some(Bytes::from(format!("init data {}", i))), true).await;
        });
    }

    while let Some(_) = set.join_next().await {}

    let duration = start.elapsed();
    println!("完成。耗时: {:?}", duration);
    println!("连接/流建立速率: {:.0} /s", conn_count as f64 / duration.as_secs_f64());
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let filter = if cfg!(debug_assertions) {
        // Debug 构建，打印所有 debug / trace
        EnvFilter::new("qs=trace")
    } else {
        // Release 构建，只打印 info 以上
        EnvFilter::new("qs=info")
    };

    // 开启日志以便观察握手过程（可选）
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    // 1. 测带宽 (1GB 数据)
    bench_throughput(1024).await;

    // 给系统一点喘息时间
    tokio::time::sleep(Duration::from_secs(2)).await;

    // // 2. 测延迟/小包 (50,000 次 ping-pong)
    // bench_pps(50_000).await;
    //
    // // 3. 测并发 Stream 建立 (10,000 个流)
    // // 注意：如果测试 Connection，需要修改代码逻辑让 Client 看起来像不同的地址
    // bench_concurrency(10_000).await;
}