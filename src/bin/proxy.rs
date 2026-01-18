use std::cell::RefCell;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use qs::{client_config, endpoint_config, server_config};
use quinn::TokioRuntime;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{TunTapInterface, Device, DeviceCapabilities, Medium, RxToken, TxToken, Checksum};
use smoltcp::socket::tcp;
use smoltcp::socket::tcp::State as TcpState;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv4Packet, TcpPacket};
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::rc::Rc;
use std::sync::Arc;
use ipstack::{IpStack, IpStackStream};
use tokio::io::{join, AsyncReadExt, AsyncWriteExt};
use tun::AbstractDevice;

// 定义 CLI 结构
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 运行服务端模式
    Server {
        /// 监听地址 (例如: 0.0.0.0:4433)
        #[arg(short, long, default_value = "0.0.0.0:4433")]
        listen: SocketAddr,
    },
    /// 运行客户端模式
    Client {
        /// 服务端地址 (例如: 127.0.0.1:4433)
        #[arg(short, long, default_value = "127.0.0.1:4433")]
        server: SocketAddr,

        /// 本地监听的 TCP 端口 (例如: 127.0.0.1:8080)
        #[arg(short, long, default_value = "127.0.0.1:8080")]
        local: SocketAddr,

        /// 想要转发到的远程目标 TCP 地址 (例如: google.com:80)
        #[arg(short, long)]
        target: String,
    },
    /// 运行服务端 (VPN 模式)
    /// 需 Root 权限: sudo ./target/release/proxy vpn-server --tun-ip 10.0.0.1
    VpnServer {
        #[arg(short, long, default_value = "0.0.0.0:4433")]
        listen: SocketAddr,
        #[arg(long, default_value = "10.0.0.1")]
        tun_ip: Ipv4Addr,
        #[arg(long, default_value = "false")]
        smoltcp: bool,
    },
    /// 运行客户端 (VPN 模式)
    /// 需 Root 权限: sudo ./target/release/proxy vpn-client --server <SERVER_IP>:4433 --tun-ip 10.0.0.2
    VpnClient {
        #[arg(short, long)]
        server: SocketAddr,
        #[arg(long, default_value = "10.0.0.2")]
        tun_ip: Ipv4Addr,
        #[arg(long, default_value = "false")]
        smoltcp: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Server { listen } => run_server(listen).await,
        Commands::Client {
            server,
            local,
            target,
        } => run_client(server, local, target).await,
        Commands::VpnServer { listen, tun_ip, smoltcp } => run_vpn_server(listen, tun_ip, smoltcp).await,
        Commands::VpnClient { server, tun_ip, smoltcp } => run_vpn_client(server, tun_ip, smoltcp).await,
    }
}

const TUN_MTU: u16 = 1120;

// ==========================================
// 辅助函数：简单的私有协议 (传输目标地址)
// 格式: [Length: u16] [Address String: bytes]
// ==========================================

async fn write_dst_addr<W: AsyncWriteExt + Unpin>(writer: &mut W, addr: &str) -> Result<()> {
    let addr_bytes = addr.as_bytes();
    let len = addr_bytes.len() as u16;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(addr_bytes).await?;
    Ok(())
}

async fn read_dst_addr<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<String> {
    let mut len_buf = [0u8; 2];
    reader.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;

    let mut addr_buf = vec![0u8; len];
    reader.read_exact(&mut addr_buf).await?;
    let addr_str = String::from_utf8_lossy(&addr_buf).to_string();
    Ok(addr_str)
}

// --- 核心逻辑: IP 搬运工 ---
// 只要连接建立，逻辑对 Client 和 Server 几乎是一样的
async fn run_datagram_tunnel(connection: quinn::Connection, tun_dev: tun::AsyncDevice) -> Result<()> {
    // 由于 tun crate 的 split 比较麻烦，我们用 Arc<AsyncDevice> + loop select 简单处理
    // 或者直接把 tun 分成 reader/writer (tun crate 支持 into_split)
    let (mut tun_write, mut tun_read) = tun_dev.split()?;

    // 任务1: TUN -> QUIC (发送 IP 包)
    let conn_tx = connection.clone();
    let t1 = tokio::spawn(async move {
        let mut buf = vec![0; TUN_MTU as usize]; // 必须小于 QUIC MTU
        loop {
            match tun_read.read(&mut buf).await {
                Ok(n) => {
                    // 使用 Datagram 发送 (不可靠，低延迟，适合 VPN)
                    // 如果包太大超过 MTU，QUIC 会报错，这里简略处理
                    let packet = bytes::Bytes::copy_from_slice(&buf[..n]);
                    if let Err(e) = conn_tx.send_datagram(packet) {
                        eprintln!("发送 Datagram (len {:?}) 失败 (可能包太大): {}", n, e);
                    }
                }
                Err(e) => {
                    eprintln!("读取 TUN 失败: {}", e);
                    break;
                }
            }
        }
    });

    // 任务2: QUIC -> TUN (接收 IP 包)
    let t2 = tokio::spawn(async move {
        loop {
            // 读取 Datagram
            match connection.read_datagram().await {
                Ok(data) => {
                    if let Err(e) = tun_write.write_all(&data).await {
                        eprintln!("写入 TUN 失败: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("连接断开: {}", e);
                    break;
                }
            }
        }
    });

    let _ = tokio::join!(t1, t2);
    Ok(())
}

// --- VPN 服务端 ---
async fn run_vpn_server(listen_addr: SocketAddr, tun_ip: Ipv4Addr, smoltcp: bool) -> Result<()> {
    // 1. 创建 TUN
    let mut config = tun::Configuration::default();
    config
        .address(tun_ip)
        .netmask((255, 255, 255, 0))
        .mtu(TUN_MTU)
        .up();

    let tun_dev = tun::create_as_async(&config).context("创建 TUN 失败 (需要 root?)")?;
    println!("🚀 Server TUN 启动: {}", tun_ip);
    println!("⚠️  请确保开启了内核转发: sysctl -w net.ipv4.ip_forward=1");
    println!("⚠️  请设置 NAT: iptables -t nat -A POSTROUTING -s 10.0.0.0/24 ! -d 10.0.0.0/24 -j MASQUERADE");

    // 2. 启动 QUIC
    let socket = UdpSocket::bind(listen_addr)?;
    let mut endpoint = quinn::Endpoint::new(
        endpoint_config(),
        Some(server_config()),
        socket,
        Arc::new(TokioRuntime),
    )?;
    endpoint.set_default_client_config(client_config());
    println!("🎧 等待客户端连接...");

    // 简单起见，这里只接受一个客户端连接，或者需要为每个客户端创建不同的 TUN/路由逻辑
    // 为了演示 IP over QUIC，我们假设是一对一，或者所有客户端共享这个 TUN (都在 10.0.0.x 子网)
    while let Some(conn) = endpoint.accept().await {
        let connection = conn.await?;
        println!("+ 客户端已连接: {}", connection.remote_address());

        // 进入隧道模式
        if smoltcp {
            println!("✨ 模式: 启用 smoltcp (TCP over Streams)");// === TCP over QUIC Streams (代理模式) ===
            // 持续接受来自客户端的 Stream
            loop {
                match connection.accept_bi().await {
                    Ok((send_stream, mut recv_stream)) => {
                        tokio::spawn(async move {
                            // 1. 读取客户端想去的目标地址
                            let target_addr = match read_dst_addr(&mut recv_stream).await {
                                Ok(addr) => addr,
                                Err(e) => { eprintln!("读取目标地址失败: {}", e); return; }
                            };

                            println!("  -> Proxy Request: {}", target_addr);

                            // 2. 服务端代替客户端连接真实目标
                            match tokio::net::TcpStream::connect(&target_addr).await {
                                Ok(mut real_tcp) => {
                                    let mut quic_stream = join(recv_stream, send_stream);
                                    // 3. 双向转发
                                    if let Err(e) = tokio::io::copy_bidirectional(&mut quic_stream, &mut real_tcp).await {
                                        // 这是一个常见的错误 (连接断开)，debug 级别即可
                                        log::debug!("代理连接断开 {}: {}", target_addr, e);
                                    }
                                },
                                Err(e) => {
                                    eprintln!("  ! 连接目标 {} 失败: {}", target_addr, e);
                                    // 可以选择写回一个错误给客户端，这里直接关闭
                                }
                            }
                        });
                    },
                    Err(e) => {
                        println!("Client 连接结束: {}", e);
                        break;
                    }
                }
            }
        } else {
            println!("✨ 模式: 原生转发 (All over Datagrams)");
            run_datagram_tunnel(connection, tun_dev).await?
        }
    }

    Ok(())
}

// --- VPN 客户端 ---
async fn run_vpn_client(server_addr: SocketAddr, tun_ip: Ipv4Addr, smoltcp: bool) -> Result<()> {
    // 1. 创建 TUN
    let mut config = tun::Configuration::default();
    config
        .address(tun_ip)
        .netmask((255, 255, 255, 0))
        .mtu(TUN_MTU)
        .up();

    let tun_dev = tun::create_as_async(&config).context("创建 TUN 失败")?;
    println!("🚀 Client TUN 启动: {}", tun_ip);

    // 2. 连接 QUIC
    let addr: SocketAddr = "0.0.0.0:0".parse()?;
    let socket = UdpSocket::bind(addr)?;
    let mut endpoint = quinn::Endpoint::new(
        endpoint_config(),
        Some(server_config()),
        socket,
        Arc::new(TokioRuntime),
    )?;
    endpoint.set_default_client_config(client_config());

    println!("⏳ 连接服务端 {}...", server_addr);
    let connection = endpoint.connect(server_addr, "localhost")?.await?;
    println!("✅ 连接成功，开始转发 IP 包...");

    // 3. 配置路由 (提示用户)
    println!("⚠️  现在请手动修改路由表，将流量指向 TUN 网卡，例如:");
    println!("   ip route add 8.8.8.8 dev tun0 (测试用)");
    println!("   或者配置默认路由 (小心不要把连 VPS 的流量也路由进去了!)");

    if smoltcp {
        println!("✨ 模式: 启用 smoltcp (TCP over Streams, UDP over Datagrams)");// === 关键修正: 使用 ipstack ===
        // ipstack 内部封装了 smoltcp 的 poll 循环和 Device 实现
        // 它需要一个 packet_receiver 和 packet_sender，或者直接传入 tun_dev 如果它实现了 AsyncRead+Write
        // tun::AsyncDevice 实现了 tokio::io::AsyncRead/Write，符合要求

        let mut ip_stack = IpStack::new(ipstack::IpStackConfig::default(), tun_dev);

        // 循环接收来自 TUN 的“连接”
        while let Some(stream_result) = ip_stack.accept().await {
            match stream_result {
                Ok(stream) => {
                    // ipstack 的流可以区分是 TCP 还是 UDP
                    match stream {
                        IpStackStream::Tcp(mut tcp_stream) => {
                            let peer_addr = tcp_stream.peer_addr();   // 本机发起请求的端口 (10.0.0.2:xxxx)
                            let local_addr = tcp_stream.local_addr(); // 用户想要连接的目标 (1.2.3.4:80)

                            println!("^ 捕获 TCP: {} -> {}", peer_addr, local_addr);

                            let connection = connection.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_client_stream(connection, tcp_stream, local_addr).await {
                                    eprintln!("流处理错误: {}", e);
                                }
                            });
                        }
                        IpStackStream::Udp(udp_packet) => {
                            // 处理 UDP (如果需要 DNS 或 QUIC over UDP 代理)
                            // 简单演示这里忽略
                            println!("丢弃 UDP 包");
                        }
                        _ => {}
                    }
                }
                Err(e) => eprintln!("ipstack accept error: {}", e),
            }
        }

        Ok(())
    } else {
        println!("✨ 模式: 原生转发 (All over Datagrams)");
        run_datagram_tunnel(connection, tun_dev).await
    }
}

// 抽离出的流处理逻辑
async fn handle_client_stream(
    conn: quinn::Connection,
    mut tun_stream: ipstack::IpStackTcpStream, // 注意这里的类型
    target_addr: SocketAddr
) -> Result<()> {
    // 1. 在 QUIC 隧道中开启一个新的流
    let (mut send_quic, recv_quic) = conn.open_bi().await?;

    // 2. 握手: 告诉服务端目标地址
    write_dst_addr(&mut send_quic, &target_addr.to_string()).await?;

    // 3. 双向转发
    // IpStackTcpStream 实现了 Tokio AsyncRead/AsyncWrite，可以直接 copy
    let mut quic_stream = join(recv_quic, send_quic);

    // 注意: ipstack 的流可能需要用 compat() 如果版本不匹配，但通常它是 tokio 兼容的
    let _ = tokio::io::copy_bidirectional(&mut tun_stream, &mut quic_stream).await?;

    Ok(())
}

// --- 服务端逻辑 ---

async fn run_server(addr: SocketAddr) -> Result<()> {
    // 2. 创建 QUIC Endpoint
    let endpoint = quinn::Endpoint::server(qs::server_config(), addr)?;
    println!("🚀 服务端监听于 UDP: {}", addr);

    // 3. 接受连接
    while let Some(conn) = endpoint.accept().await {
        tokio::spawn(async move {
            let remote_addr = conn.remote_address();
            println!("+ 新连接来自: {}", remote_addr);

            let connection = match conn.await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("连接握手失败: {}", e);
                    return;
                }
            };

            // 4. 处理该连接中的流
            while let Ok((send_stream, mut recv_stream)) = connection.accept_bi().await {
                tokio::spawn(async move {
                    // 读取协议头：目标地址长度 (u16)
                    let mut len_buf = [0u8; 2];
                    if recv_stream.read_exact(&mut len_buf).await.is_err() {
                        return;
                    }
                    let len = u16::from_be_bytes(len_buf) as usize;

                    // 读取目标地址字符串
                    let mut addr_buf = vec![0u8; len];
                    if recv_stream.read_exact(&mut addr_buf).await.is_err() {
                        return;
                    }
                    let target_str = String::from_utf8_lossy(&addr_buf).to_string();

                    println!("  -> 请求代理到: {}", target_str);

                    // 连接目标 TCP
                    match tokio::net::TcpStream::connect(&target_str).await {
                        Ok(mut tcp_stream) => {
                            // if let Err(e) = tcp_stream.set_nodelay(true) {
                            //     eprintln!("  ! 警告: 无法设置 TCP_NODELAY: {}", e);
                            // }

                            // 双向拷贝数据
                            // split TCP stream to use allow separate read/write in copy_bidirectional
                            let mut quic_stream = join(recv_stream, send_stream);

                            // 代理数据：TCP <-> QUIC
                            let _ = tokio::io::copy_bidirectional_with_sizes(
                                &mut tcp_stream,
                                &mut quic_stream,
                                1 << 20,
                                1 << 20,
                            )
                            .await;
                        }
                        Err(e) => {
                            eprintln!("  ! 无法连接到目标 TCP {}: {}", target_str, e);
                        }
                    }
                });
            }
        });
    }

    Ok(())
}

// --- 客户端逻辑 ---

async fn run_client(server_addr: SocketAddr, local_addr: SocketAddr, target: String) -> Result<()> {
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())?;
    endpoint.set_default_client_config(qs::client_config());

    println!("⏳ 正在连接到服务端 QUIC {}...", server_addr);

    // 2. 建立 QUIC 连接
    // 在这个简单示例中，我们建立一个长连接供所有 TCP 使用
    // 如果连接断开，需要重启客户端 (生产环境需要重连逻辑)
    let connection = endpoint
        .connect(server_addr, "localhost")?
        .await
        .context("无法连接到服务端")?;

    println!("✅ QUIC 连接已建立");
    println!("🎧 本地 TCP 监听于 {}", local_addr);
    println!("👉 流量转发目标: {}", target);

    // 3. 监听本地 TCP
    let listener = tokio::net::TcpListener::bind(local_addr).await?;

    loop {
        let (mut socket, _) = listener.accept().await?;
        // if let Err(e) = socket.set_nodelay(true) {
        //     eprintln!("无法设置本地 TCP_NODELAY: {}", e);
        // }

        let connection = connection.clone();
        let target = target.clone();

        tokio::spawn(async move {
            // 4. 为每个 TCP 连接打开一个新的 QUIC 流
            match connection.open_bi().await {
                Ok((mut send_stream, recv_stream)) => {
                    // 发送自定义协议头: [len(u16)][address_bytes]
                    let target_bytes = target.as_bytes();
                    let len = target_bytes.len() as u16;

                    if let Err(e) = send_stream.write_all(&len.to_be_bytes()).await {
                        eprintln!("写入长度失败: {}", e);
                        return;
                    }
                    if let Err(e) = send_stream.write_all(target_bytes).await {
                        eprintln!("写入地址失败: {}", e);
                        return;
                    }

                    // 5. 进行双向转发
                    let mut quic_stream = join(recv_stream, send_stream);

                    let _ = tokio::io::copy_bidirectional_with_sizes(
                        &mut socket,
                        &mut quic_stream,
                        1 << 20,
                        1 << 20,
                    )
                    .await;
                }
                Err(e) => eprintln!("打开 QUIC 流失败: {}", e),
            }
        });
    }
}
