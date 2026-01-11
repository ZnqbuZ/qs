use crate::gateway::quic::cmd::{QuicCmd, QuicCmdTx};
use crate::gateway::quic::driver::{QuicDriver, QuicStreamCtxRx};
use crate::gateway::quic::evt::{QuicNetEvt, QuicNetEvtRx};
use crate::gateway::quic::packet::{QuicPacket, QuicPacketMargins};
use crate::gateway::quic::stream::QuicStream;
use crate::gateway::quic::{switched_channel, AtomicSwitch};
use anyhow::{anyhow, Error};
use bytes::Bytes;
use derive_more::Constructor;
use quinn_plaintext::{client_config, server_config};
use quinn_proto::congestion::BbrConfig;
use quinn_proto::{ClientConfig, Endpoint, EndpointConfig, TransportConfig, VarInt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::select;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::sleep_until;

#[derive(Debug)]
pub struct QuicCtrl {
    cmd_tx: QuicCmdTx,packet_tx: QuicCmdTx,   // [新增] 专用通道，用于发送 InputPacket
}

impl QuicCtrl {pub fn new(cmd_tx: QuicCmdTx, packet_tx: QuicCmdTx) -> Self {
    Self { cmd_tx, packet_tx }
}#[inline]
pub async fn send(&self, packet: QuicPacket) -> Result<(), Error> {
    // [修改] 发送包时走 packet_tx，避免被 stream write 阻塞
    self.packet_tx
        .send(QuicCmd::InputPacket(packet))
        .await
        .map_err(|e| Error::msg(format!("Failed to send QuicCmd::PacketIncoming: {:?}", e)))
}

    #[inline]
    pub async fn connect(
        &self,
        addr: SocketAddr,
        data: Option<Bytes>,
        wait: bool,
    ) -> Result<QuicStream, Error> {
        let (stream_tx, stream_rx) = oneshot::channel();
        self.cmd_tx
            .send(QuicCmd::OpenBiStream {
                addr,
                data,
                stream_tx,
            })
            .await?;
        let mut stream = QuicStream::new(stream_rx.await??, self.cmd_tx.clone(), false);
        if wait {
            stream.ready().await?;
        }
        Ok(stream)
    }
}

#[derive(Debug)]
pub struct QuicPacketRx {
    net_evt_rx: QuicNetEvtRx,
    packet_margins: QuicPacketMargins,
}

impl QuicPacketRx {
    #[inline]
    pub async fn recv(&mut self) -> Option<QuicPacket> {
        match self.net_evt_rx.recv().await? {
            QuicNetEvt::OutputPacket(packet) => Some(packet),
        }
    }

    #[inline]
    pub fn packet_margins(&self) -> QuicPacketMargins {
        self.packet_margins
    }
}

#[derive(Debug)]
pub struct QuicStreamRx {
    cmd_tx: QuicCmdTx,
    incoming_stream_rx: QuicStreamCtxRx,
}

impl QuicStreamRx {
    #[inline]
    pub async fn recv(&mut self) -> Option<QuicStream> {
        Some(QuicStream::new(
            self.incoming_stream_rx.recv().await?,
            self.cmd_tx.clone(),
            true,
        ))
    }

    #[inline]
    pub fn switch(&self) -> &AtomicSwitch {
        self.incoming_stream_rx.switch.as_ref()
    }
}

pub struct QuicEndpoint {
    endpoint: Option<Endpoint>,
    client_config: ClientConfig,
    ctrl: Option<Arc<QuicCtrl>>,
    tasks: JoinSet<()>,
}

impl QuicEndpoint {
    pub fn new() -> Self {
        let mut server_config = server_config();
        server_config.transport = {
            let mut config = TransportConfig::default();

            config.stream_receive_window(VarInt::from_u32(10 * 1024 * 1024));
            config.receive_window(VarInt::from_u32(15 * 1024 * 1024));

            config.max_concurrent_bidi_streams(VarInt::from_u32(1024));
            config.max_concurrent_uni_streams(VarInt::from_u32(1024));

            config.congestion_controller_factory(Arc::new(BbrConfig::default()));

            config.keep_alive_interval(Some(Duration::from_secs(5)));
            config.max_idle_timeout(Some(VarInt::from_u32(30_000).into()));

            Arc::new(config)
        };

        let endpoint_config = EndpointConfig::default();

        let endpoint = Endpoint::new(
            Arc::from(endpoint_config),
            Some(Arc::from(server_config)),
            false,
            None,
        );

        Self::with_endpoint(endpoint, client_config())
    }

    #[inline]
    pub fn with_endpoint(endpoint: Endpoint, client_config: ClientConfig) -> Self {
        Self {
            endpoint: Some(endpoint),
            client_config,
            ctrl: None,
            tasks: JoinSet::new(),
        }
    }

    pub fn run(
        &mut self,
        packet_margins: QuicPacketMargins,
    ) -> Option<(QuicPacketRx, QuicStreamRx)> {
        self.endpoint.as_ref()?;
        let (cmd_tx, mut cmd_rx) = mpsc::channel(500_000); // 依然大容量，用于 Write
        let (packet_tx, mut packet_rx2) = mpsc::channel(500_000); // 依然大容量，用于 Packet Input
        let (net_evt_tx, net_evt_rx) = mpsc::channel(500_000);
        let (incoming_stream_tx, incoming_stream_rx) = switched_channel(128);

        self.ctrl = Some(QuicCtrl::new(cmd_tx.clone(), packet_tx).into());
        let packet_rx = QuicPacketRx {
            net_evt_rx,
            packet_margins,
        };
        let stream_rx = QuicStreamRx {
            cmd_tx: cmd_tx.clone(),
            incoming_stream_rx,
        };

        let mut drv = QuicDriver::new(
            self.endpoint.take().unwrap(),
            self.client_config.clone(),
            net_evt_tx.clone(),
            incoming_stream_tx.clone(),
            packet_margins,
        );

        self.tasks.spawn(async move {
            loop {
                let min_timeout = drv
                    .min_timeout()
                    .unwrap_or(Instant::now() + Duration::from_secs(60));

                select! {
                    // [核心修改] biased 模式 + 优先处理 packet_rx
                    // 这样即使 cmd_rx 堆积如山，ACK 包也能立刻被处理，维持连接活性
                    biased;

                    Some(cmd) = packet_rx2.recv() => {
                        drv.execute(cmd);
                        // 贪婪消费 Packets
                        for _ in 0..100 {
                            match packet_rx2.try_recv() {
                                Ok(c) => drv.execute(c),
                                Err(_) => break,
                            }
                        }
                        drv.flush_io();
                    },

                    Some(cmd) = cmd_rx.recv() => {
                        drv.execute(cmd);
                        // 贪婪消费 Commands (Write)
                        for _ in 0..100 {
                            match cmd_rx.try_recv() {
                                Ok(c) => drv.execute(c),
                                Err(_) => break,
                            }
                        }
                        drv.flush_io();
                    },
                    _ = sleep_until(min_timeout.into()) => drv.handle_timeout(),
                }
            }
        });

        Some((packet_rx, stream_rx))
    }

    #[inline]
    pub fn ctrl(&self) -> Result<Arc<QuicCtrl>, Error> {
        self.ctrl.clone().ok_or(anyhow!(
            "Failed to get QUIC controller. Is QUIC endpoint running?"
        ))
    }
}
