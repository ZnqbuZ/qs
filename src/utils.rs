use chrono::Utc;
use quinn::congestion::BbrConfig;
use quinn::ClientConfig;
use quinn::EndpointConfig;
use quinn::ServerConfig;
use quinn::TransportConfig;
use quinn::VarInt;
use quinn_proto::QlogConfig;
use rand::distr::Alphanumeric;
use rand::{rng, Rng};
use std::fs::File;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::info;

const QLOG: bool = false;

pub fn transport_config() -> Arc<TransportConfig> {
    let qlog_stream = if !QLOG {
        None
    } else {
        let qlog_path = format!(
            "/home/luna/qlog/qs-{}-{}.qlog",
            Utc::now().format("%H%M%S.%3f"),
            rng()
                .sample_iter(Alphanumeric)
                .take(4)
                .map(char::from)
                .collect::<String>()
        );
        let qlog_path = Path::new(&qlog_path);
        let qlog_file = Box::new(File::create(&*qlog_path).unwrap());
        let mut qlog_config = QlogConfig::default();
        qlog_config.writer(qlog_file);
        Some(qlog_config.into_stream().unwrap())
    };

    // TODO: subject to change
    let mut config = TransportConfig::default();

    config
        // .qlog_stream(qlog_stream)
        .stream_receive_window(VarInt::from_u32(64 * 1024 * 1024))
        .receive_window(VarInt::from_u32(1024 * 1024 * 1024))
        .send_window(1024 * 1024 * 1024)
        .max_concurrent_bidi_streams(VarInt::from_u32(1024))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .max_idle_timeout(Some(VarInt::from_u32(30_000).into()))
        .initial_mtu(1200)
        .min_mtu(1200)
        .enable_segmentation_offload(true)
        .congestion_controller_factory(Arc::new(BbrConfig::default()))
        .datagram_receive_buffer_size(Some(1024 * 1024 * 1024))
        .datagram_send_buffer_size(1024 * 1024 * 1024);

    Arc::new(config)
}

pub fn server_config() -> ServerConfig {
    let mut config = quinn_plaintext::server_config();
    config.transport_config(transport_config());
    config
}

pub fn client_config() -> ClientConfig {
    let mut config = quinn_plaintext::client_config();
    config.transport_config(transport_config());
    config
}

pub fn endpoint_config() -> EndpointConfig {
    let mut config = EndpointConfig::default();
    config.max_udp_payload_size(65527).unwrap();
    config
}

static NEXT_STREAM_ID: AtomicUsize = AtomicUsize::new(0);
pub static STREAM_MONITOR: Lazy<DashMap<usize, Arc<StreamStats>>> = Lazy::new(|| DashMap::new());

pub fn run_stream_monitor() {
    tokio::spawn(async {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            interval.tick().await;

            if STREAM_MONITOR.is_empty() {
                continue;
            }

            println!("--- 实时速率监控 (活跃流: {}) ---", STREAM_MONITOR.len());

            let mut snapshot = Vec::new();

            for item in STREAM_MONITOR.iter() {
                let stats = item.value();
                let curr_rx = stats.total_rx.load(Ordering::Relaxed);
                let curr_tx = stats.total_tx.load(Ordering::Relaxed);
                let curr_pending = stats.total_write_pending.load(Ordering::Relaxed);

                let prev_rx = stats.last_rx.swap(curr_rx, Ordering::Relaxed);
                let prev_tx = stats.last_tx.swap(curr_tx, Ordering::Relaxed);
                let prev_pending = stats.last_write_pending.swap(curr_pending, Ordering::Relaxed);

                let rate_rx = curr_rx.saturating_sub(prev_rx);
                let rate_tx = curr_tx.saturating_sub(prev_tx);
                let delta_pending = curr_pending.saturating_sub(prev_pending);


                snapshot.push((stats.id, stats.name.clone(), rate_rx + rate_tx, rate_rx, rate_tx, delta_pending));
            }

            snapshot.sort_by_key(|k| k.0);

            let to_mbps = |bytes: u64| -> String {
                let bits = bytes as f64 * 8.0;
                let mbps = bits / 1_000_000.0; // 网络常用 1000 进制，如果习惯系统进制可用 1024.0 * 1024.0
                if mbps < 0.01 && bytes > 0 {
                    format!("{:.4} Mbps", mbps) // 极小流量保留更多小数
                } else {
                    format!("{:.2} Mbps", mbps) // 正常保留两位小数
                }
            };

            for (_, tag, total, rx, tx, pending) in snapshot {
                println!("[{}]: {} (Rx: {}, Tx: {}) | Write Blocked: {} s^-1", tag, to_mbps(total), to_mbps(rx), to_mbps(tx), pending);
            }
            println!("--------------------------------");
        }
    });
}

#[derive(Debug)]
pub struct StreamStats {
    pub id: usize,
    pub name: String,        // 标识：如 "192.168.1.5 <-> 8.8.8.8"
    pub total_rx: AtomicU64, // 总接收字节
    pub total_tx: AtomicU64, // 总发送字节
    pub last_rx: AtomicU64,  // 上一次采样的接收字节（用于算速率）
    pub last_tx: AtomicU64,  // 上一次采样的发送字节
    pub total_write_pending: AtomicUsize, // 总共遇到了多少次写阻塞
    pub last_write_pending: AtomicUsize,  // 上一次采样的阻塞次数（用于计算增量）
}

pub struct MonitoredStream<T> {
    inner: T,
    stats: Arc<StreamStats>,
}

impl<T> MonitoredStream<T> {
    pub fn new(inner: T, name: &str) -> Self {
        let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
        let stats = Arc::new(StreamStats {
            id,
            name: name.to_string(),
            total_rx: AtomicU64::new(0),
            total_tx: AtomicU64::new(0),
            last_rx: AtomicU64::new(0),
            last_tx: AtomicU64::new(0),
            total_write_pending: AtomicUsize::new(0),
            last_write_pending: AtomicUsize::new(0),
        });
        STREAM_MONITOR.insert(id, stats.clone());
        Self { inner, stats }
    }
}

impl<T> Drop for MonitoredStream<T> {
    fn drop(&mut self) {
        info!("Stream dropped, stats: {:?}", self.stats);
        STREAM_MONITOR.remove(&self.stats.id);
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for MonitoredStream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        match poll {
            Poll::Ready(Ok(())) => {
                let n = (buf.filled().len() - before) as u64;
                self.stats.total_rx.fetch_add(n, Ordering::Relaxed);
            }
            Poll::Pending => {
                self.stats.total_write_pending.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        poll
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for MonitoredStream<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &poll {
            self.stats.total_tx.fetch_add(*n as u64, Ordering::Relaxed);
        }
        poll
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

