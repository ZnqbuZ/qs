use quinn::VarInt;
use quinn::TransportConfig;
use quinn::congestion::BbrConfig;
use std::sync::Arc;
use std::time::Duration;

pub fn transport_config() -> Arc<TransportConfig> {

    let mut transport_config = TransportConfig::default();

    // TODO: subject to change
    transport_config.stream_receive_window(VarInt::from_u32(64 * 1024 * 1024));
    transport_config.receive_window(VarInt::from_u32(1024 * 1024 * 1024));
    transport_config.send_window(1024 * 1024 * 1024);

    transport_config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
    transport_config.max_concurrent_uni_streams(VarInt::from_u32(0));

    transport_config.datagram_receive_buffer_size(None);

    transport_config.keep_alive_interval(Some(Duration::from_secs(5)));
    transport_config.max_idle_timeout(Some(VarInt::from_u32(30_000).into()));

    transport_config.initial_mtu(1200);
    transport_config.min_mtu(1200);

    transport_config.enable_segmentation_offload(false);

    transport_config.congestion_controller_factory(Arc::new(BbrConfig::default()));

    Arc::new(transport_config)
}