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
use std::sync::Arc;
use std::time::Duration;

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
