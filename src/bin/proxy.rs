use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use qs::{client_config, endpoint_config, server_config};
use quinn::TokioRuntime;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{IpAddress, IpCidr, IpProtocol, Ipv4Packet, TcpPacket};
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use tokio::io::{join, AsyncReadExt, AsyncWriteExt};

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
        Commands::VpnServer { listen, tun_ip } => run_vpn_server(listen, tun_ip).await,
        Commands::VpnClient { server, tun_ip, smoltcp } => run_vpn_client(server, tun_ip, smoltcp).await,
    }
}

const TUN_MTU: u16 = 1120;

// --- 1. 修正 Device 实现 (解决 E0276) ---

struct TunBufferDevice<'a> {
    rx_buf: Option<&'a mut [u8]>,
    tx_queue: &'a mut VecDeque<Vec<u8>>,
    mtu: usize,
}

// 这里的关键是 Token 的定义不要引入多余的生命周期约束
struct RxBufferToken<'a>(&'a mut [u8]);

impl<'a> RxToken for RxBufferToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.0)
    }
}

struct TxBufferToken<'a> { queue: &'a mut VecDeque<Vec<u8>> }
impl<'a> TxToken for TxBufferToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let res = f(&mut buf);
        self.queue.push_back(buf);
        res
    }
}

impl<'a> Device for TunBufferDevice<'a> {
    type RxToken<'token> = RxBufferToken<'token> where Self: 'token;
    type TxToken<'token> = TxBufferToken<'token> where Self: 'token;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.rx_buf.take().map(|buf| {
            (RxBufferToken(buf), TxBufferToken { queue: self.tx_queue })
        })
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxBufferToken { queue: self.tx_queue })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = self.mtu;
        caps.medium = Medium::Ip;
        caps
    }
}

// --- 2. 修正主循环逻辑 (解决 E0499) ---

async fn run_smoltcp_tunnel(connection: quinn::Connection, tun_dev: tun::AsyncDevice) -> Result<()> {
    let (mut tun_write, mut tun_read) = tun_dev.split()?;
    let mut tun_buf = vec![0u8; TUN_MTU as usize];

    // 初始化 smoltcp
    let mut device_config = Config::new(smoltcp::wire::HardwareAddress::Ip);
    device_config.random_seed = rand::random();

    // 初始化 Interface
    // 注意：TunBufferDevice 需要在 loop 里动态构建，因为它是对 tun_buf 的借用
    // 这里我们先创建一个空的 socket set
    let mut iface = Interface::new(device_config, &mut TunBufferDevice {
        rx_buf: None, tx_queue: &mut VecDeque::new(), mtu: TUN_MTU as usize
    }, Instant::now());

    iface.update_ip_addrs(|ips| { ips.push(IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0)).unwrap(); });

    let mut sockets = SocketSet::new(vec![]);

    // Flow 结构体用于管理 QUIC 流
    struct Flow {
        send: quinn::SendStream,
        recv: quinn::RecvStream,
    }
    let mut flows: HashMap<smoltcp::iface::SocketHandle, Flow> = HashMap::new();
    let mut tx_to_tun_queue: VecDeque<Vec<u8>> = VecDeque::new();

    loop {
        // --- 阶段 1: IO 输入 (tokio::select) ---
        // 在这一步，我们只收集数据，不要去碰 sockets 或 iface 的内部状态

        let mut tun_input: Option<usize> = None;
        let mut should_poll = false;

        tokio::select! {
            // A. 读取 TUN
            res = tun_read.read(&mut tun_buf) => {
                match res {
                    Ok(n) => tun_input = Some(n),
                    Err(_) => break,
                }
            }
            // B. 读取 UDP Datagram (处理非 TCP 流量)
            res = connection.read_datagram() => {
                if let Ok(data) = res {
                    let _ = tun_write.write_all(&data).await;
                } else {
                    break;
                }
            }
            // C. 简单的定时器，保证 loop 滚动以驱动 smoltcp 的重传和超时
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                should_poll = true;
            }
        }

        let timestamp = Instant::now();

        // --- 阶段 2: 将 TUN 数据注入 smoltcp 并执行 Poll ---
        // 这是唯一一次借用 sockets 进行全局更新的地方

        { // 作用域开始
            // 1. 预处理：先通过 buffer 引用进行检查，不消耗所有权
            let mut consumed_by_smoltcp = false;

            // 使用 if let 简化逻辑，直接借用 tun_buf，而不是创建 rx_slice
            if let Some(n) = tun_input {
                let packet_slice = &tun_buf[..n]; // 这里是不可变借用，安全

                if let Ok(ip) = Ipv4Packet::new_checked(packet_slice) {
                    if ip.next_header() == IpProtocol::Tcp {
                        consumed_by_smoltcp = true;

                        // 检查 SYN 逻辑：依然使用 packet_slice (不可变借用)
                        if let Ok(tcp) = TcpPacket::new_checked(ip.payload()) {
                            if tcp.syn() && !tcp.ack() {
                                let src = ip.src_addr();
                                let dst = ip.dst_addr();
                                let dst_port = tcp.dst_port();
                                let target_endpoint = smoltcp::wire::IpEndpoint::new(IpAddress::Ipv4(dst), dst_port);

                                // 检查 socket 是否存在
                                let exists = sockets.iter().any(|(_h, s)| {
                                    if let smoltcp::socket::Socket::Tcp(t) = s {
                                        t.local_endpoint() == Some(target_endpoint)
                                    } else { false }
                                });

                                if !exists {
                                    let rx = tcp::SocketBuffer::new(vec![0; 65535]);
                                    let tx = tcp::SocketBuffer::new(vec![0; 65535]);
                                    let mut s = tcp::Socket::new(rx, tx);
                                    if s.listen(target_endpoint).is_ok() {
                                        sockets.add(s);
                                    }
                                }
                            }
                        }
                    }
                }

                // 非 TCP 流量直接转发
                if !consumed_by_smoltcp {
                    let data = bytes::Bytes::copy_from_slice(packet_slice);
                    let _ = connection.send_datagram(data);
                }
            }

            // 2. 构造 Device：只有在这里才进行可变借用
            // 如果是 TCP，才把 buf 的可变引用传给 rx_buf
            let rx_slice_mut = if consumed_by_smoltcp {
                tun_input.map(|n| &mut tun_buf[..n])
            } else {
                None
            };

            let mut device = TunBufferDevice {
                rx_buf: rx_slice_mut, // 此时才发生 Move
                tx_queue: &mut tx_to_tun_queue,
                mtu: TUN_MTU as usize,
            };

            // 3. 执行 Poll
            iface.poll(timestamp, &mut device, &mut sockets);

        } // 作用域结束，device 销毁，tun_buf 借用释放
        // 现在我们可以安全地遍历 sockets 了。

        // --- 阶段 3: Socket 与 QUIC 数据交换 ---

        let mut to_remove = Vec::new();

        // 这里只遍历，不调用 iface.poll()
        for (handle, socket) in sockets.iter_mut() {
            let socket = match socket { smoltcp::socket::Socket::Tcp(s) => s, _ => continue };

            // 3.1 建立新流
            if socket.state() == tcp::State::Established && !flows.contains_key(&handle) {
                if let Some(local) = socket.local_endpoint() {
                    let target = format!("{}:{}", local.addr, local.port);
                    if let Ok((mut tx, rx)) = connection.open_bi().await {
                        // 发送头
                        let b = target.as_bytes();
                        let _ = tx.write_u16(b.len() as u16).await;
                        let _ = tx.write_all(b).await;
                        flows.insert(handle, Flow { send: tx, recv: rx });
                    } else {
                        socket.abort();
                    }
                }
            }

            if let Some(flow) = flows.get_mut(&handle) {
                // 3.2 smoltcp -> QUIC
                if socket.can_recv() {
                    while let Ok(data) = socket.recv(|b| (b.len(), b.to_vec())) {
                        if data.is_empty() { break; }
                        let _ = flow.send.write_all(&data).await;
                    }
                }

                // 3.3 QUIC -> smoltcp
                // 这是一个 hack：为了避免阻塞 loop，我们只尝试读一次，或者用 timeout(0)
                if socket.can_send() {
                    let mut buf = [0u8; 4096];
                    // 使用极短的 timeout 模拟 try_read
                    if let Ok(Ok(n)) = tokio::time::timeout(std::time::Duration::from_micros(1), flow.recv.read(&mut buf)).await {
                        if let Some(n) = n {
                            // 写入 Socket Buffer
                            socket.send_slice(&buf[..n]).ok();
                            // 注意：这里写入了数据，但不会立即触发 TCP ACK，
                            // ACK 会在下一次循环的 iface.poll() 中发出。这是设计预期的。
                        } else {
                            // EOF
                            socket.close();
                        }
                    }
                }

                if socket.state() == tcp::State::Closed {
                    let _ = flow.send.finish();
                    to_remove.push(handle);
                }
            }
        }

        // 清理
        for h in to_remove {
            sockets.remove(h);
            flows.remove(&h);
        }

        // --- 阶段 4: 发送 Poll 产生的包到 TUN ---
        // iface.poll() 可能会产生回包（ACK等），存放在 tx_to_tun_queue 中
        while let Some(packet) = tx_to_tun_queue.pop_front() {
            let _ = tun_write.write_all(&packet).await;
        }
    }

    Ok(())
}

// --- 核心逻辑: IP 搬运工 ---
// 只要连接建立，逻辑对 Client 和 Server 几乎是一样的
async fn run_tunnel(connection: quinn::Connection, tun_dev: tun::AsyncDevice) -> Result<()> {
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
async fn run_vpn_server(listen_addr: SocketAddr, tun_ip: Ipv4Addr) -> Result<()> {
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
    if let Some(conn) = endpoint.accept().await {
        let connection = conn.await?;
        println!("+ 客户端已连接: {}", connection.remote_address());

        // 进入隧道模式
        run_tunnel(connection, tun_dev).await?;
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
        println!("✨ 模式: 启用 smoltcp (TCP over Streams, UDP over Datagrams)");
        run_smoltcp_tunnel(connection, tun_dev).await
    } else {
        println!("✨ 模式: 原生转发 (All over Datagrams)");
        run_tunnel(connection, tun_dev).await
    }
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
