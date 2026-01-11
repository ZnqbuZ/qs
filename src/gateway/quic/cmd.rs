use crate::gateway::quic::packet::QuicPacket;
use crate::gateway::quic::stream::QuicStreamHdl;
use crate::gateway::quic::QuicStreamCtx;
use anyhow::Error;
use bytes::Bytes;
use quinn_proto::ConnectionHandle;
use std::net::SocketAddr;
use tokio::sync::{mpsc, oneshot};
use derive_more::Debug;

// TODO: add more commands
#[derive(Debug)]
pub(crate) enum QuicCmd {
    // Net
    InputPacket(
        #[debug(ignore)]
        QuicPacket
    ),
    // Connection
    OpenBiStream {
        addr: SocketAddr,
        #[debug(ignore)]
        data: Option<Bytes>,
        stream_tx: oneshot::Sender<Result<QuicStreamCtx, Error>>,
    },
    CloseConnection {
        conn_hdl: ConnectionHandle,
        error_code: u32,
        reason: Bytes,
    },
    // Stream
    StreamWrite {
        stream_hdl: QuicStreamHdl,
        #[debug(ignore)]
        data: Bytes,
        fin: bool,
    },
    StreamUnblockRead {
        stream_hdl: QuicStreamHdl,
    },
    StopStream {
        stream_hdl: QuicStreamHdl,
        error_code: u32,
    },
    ResetStream {
        stream_hdl: QuicStreamHdl,
        error_code: u32,
    },
}

pub(crate) type QuicCmdTx = mpsc::Sender<QuicCmd>;
pub(crate) type QuicCmdRx = mpsc::Receiver<QuicCmd>;
