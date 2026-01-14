use crate::gateway::quic::utils::BufPool;
use bytes::Bytes;
use futures::task::AtomicWaker;
use quinn_proto::{Connection, Event, ReadError, ReadableError, StreamEvent, StreamId, WriteError};
use rtrb::{Consumer, Producer};
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

const QUIC_STREAM_RECV_BUF_CAP: usize = 1024;
const QUIC_STREAM_RECV_UNBLOCK_THRESHOLD: usize = QUIC_STREAM_RECV_BUF_CAP * 3 / 4;

const QUIC_STREAM_SEND_BUF_CAP: usize = 1024;
const QUIC_STREAM_SEND_UNBLOCK_THRESHOLD: usize = QUIC_STREAM_SEND_BUF_CAP * 3 / 4;

enum StreamEvt {
    RecvUnblocked(StreamId),
    RecvClosed(StreamId),
    SendUnblocked(StreamId),
    SendClosed(StreamId),
}

struct RecvStream {
    id: StreamId,
    // Upstream write until recv is **full**
    recv: Consumer<Bytes>,
    // Upstream immediately call this when recv becomes non-empty from empty
    waker: Arc<AtomicWaker>,
    pending: Option<Bytes>,
    // Send a message to upstream when recv becomes (<=THRESHOLD) full from (>THRESHOLD) full
    evt_tx: mpsc::UnboundedSender<StreamEvt>,
    // Whether upstream is blocked
    blocked: Arc<AtomicBool>,

    closed: Arc<AtomicBool>,
}

impl AsyncRead for RecvStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        let mut slots = this.recv.slots();

        if slots == 0 && this.pending.is_none() {
            if this.closed.load(Ordering::Acquire) {
                return Poll::Ready(Ok(()));
            }
            this.waker.register(cx.waker());
            fence(Ordering::SeqCst);
            if this.recv.is_empty() {
                return Poll::Pending;
            }
        }

        let mut chunks = this.recv.read_chunk(slots).unwrap().into_iter();
        loop {
            if let Some(pending) = &mut this.pending {
                if !pending.is_empty() {
                    let len = min(buf.remaining(), pending.len());
                    buf.put_slice(&pending.split_to(len));
                }
                if !pending.is_empty() {
                    break;
                }
                this.pending = None;
            }

            debug_assert!(this.pending.is_none());

            let Some(chunk) = chunks.next() else {
                break;
            };
            this.pending = Some(chunk);
            slots -= 1;
        }
        drop(chunks);

        if slots <= QUIC_STREAM_RECV_UNBLOCK_THRESHOLD
            && !this.closed.load(Ordering::Acquire)
            && this.blocked.load(Ordering::Relaxed)
            && this.blocked.swap(false, Ordering::SeqCst)
            && let Err(_) = this.evt_tx.send(StreamEvt::RecvUnblocked(this.id))
        {
            this.closed.store(true, Ordering::Release);
        }

        Poll::Ready(Ok(()))
    }
}

struct SendStream {
    id: StreamId,
    // This write until send is full
    send: Producer<Bytes>,
    // Upstream call this when send becomes (75%) non-full from full
    waker: Arc<AtomicWaker>,
    // Send a message to downstream if it's blocked and the send buffer is more than 25% full
    evt_tx: mpsc::UnboundedSender<StreamEvt>,
    // Whether downstream is blocked
    blocked: Arc<AtomicBool>,

    pool: BufPool,

    closed: Arc<AtomicBool>,
}

impl AsyncWrite for SendStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.poll_write_vectored(cx, &[IoSlice::new(buf)])
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.blocked.swap(false, Ordering::SeqCst)
            && let Err(_) = self.evt_tx.send(StreamEvt::SendUnblocked(self.id))
        {
            self.closed.store(true, Ordering::Release);
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let _ = self.evt_tx.send(StreamEvt::SendClosed(self.id));
        self.closed.store(true, Ordering::Release);
        Poll::Ready(Ok(()))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();

        if this.closed.load(Ordering::Acquire) {
            return Poll::Ready(Ok(0));
        }

        if this.send.is_full() {
            this.waker.register(cx.waker());
            if this.send.is_full() {
                return Poll::Pending;
            }
        }

        let slots = this.send.slots();
        let n = min(bufs.len(), slots);
        let mut len = 0;
        let bufs = this.pool.buf_vectored(&bufs[..n], None).map(|buf| {
            len += buf.len();
            buf.freeze()
        });
        this.send
            .write_chunk_uninit(n)
            .unwrap()
            .fill_from_iter(bufs);

        // once `blocked` is true, downstream cannot change it back to false
        // the next poll_write (or poll_flush) wakes downstream if the value of `blocked` is lagged
        // I suppose poll_flush will always be called
        if ((n == QUIC_STREAM_SEND_BUF_CAP && this.blocked.swap(false, Ordering::SeqCst))
            || (slots - n <= QUIC_STREAM_SEND_UNBLOCK_THRESHOLD
            && this.blocked.swap(false, Ordering::Relaxed)))
            && let Err(_) = this.evt_tx.send(StreamEvt::SendUnblocked(this.id))
        {
            this.closed.store(true, Ordering::Release);
        }

        Poll::Ready(Ok(len))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }
}

struct RecvStreamCtx {
    recv: Producer<Bytes>,
    waker: Arc<AtomicWaker>,
    blocked: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

struct SendStreamCtx {
    send: Consumer<Bytes>,
    waker: Arc<AtomicWaker>,
    blocked: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
}

struct Runner {
    evt_rx: mpsc::UnboundedReceiver<StreamEvt>,
}

impl Runner {
    fn run(&mut self, mut conn: Connection) {
        let mut writable_streams = HashSet::new();
        let mut readable_streams = HashSet::new();
        let mut send_streams = HashMap::<StreamId, SendStreamCtx>::new();
        let mut recv_streams = HashMap::<StreamId, RecvStreamCtx>::new();
        let mut blocked_send_streams = HashSet::new();
        let mut blocked_recv_streams = HashSet::new();
        // event loop
        loop {
            while let Some(evt) = conn.poll() {
                match evt {
                    Event::Stream(StreamEvent::Writable { id }) => {
                        writable_streams.insert(id);
                    }
                    Event::Stream(StreamEvent::Readable { id }) => {
                        readable_streams.insert(id);
                    }
                    _ => todo!(),
                }
            }

            while let Ok(evt) = self.evt_rx.try_recv() {
                match evt {
                    StreamEvt::SendUnblocked(id) => {
                        blocked_send_streams.remove(&id);
                    }
                    StreamEvt::SendClosed(id) => {
                        // if this stream is still open in quinn, wait for the next Writable event (or closed event)
                        // else delete it
                    }
                    StreamEvt::RecvUnblocked(id) => {
                        blocked_recv_streams.remove(&id);
                    }
                    StreamEvt::RecvClosed(id) => {
                        // delete this stream from quinn and those lists
                        // as I have nowhere to send the data
                    }
                    _ => todo!(),
                }
            }

            readable_streams.retain(|id| {
                let ctx = recv_streams.get_mut(id).unwrap();
                let mut stream = conn.recv_stream(*id);
                loop {
                    if ctx.closed.load(Ordering::Relaxed) {
                        /* downstream closed, close this stream as I can do nothing more */
                        /* wait for the RecvClosed signal, delete this stream there */
                        break false;
                    }

                    /* if ctx.recv is full, I'm blocked, but this stream is still readable */
                    /* downstream is too slow, wait for it consuming at least 25% of the buffer */
                    if ctx.recv.is_full() {
                        ctx.blocked.store(true, Ordering::Relaxed);
                        blocked_recv_streams.insert(*id);
                        break true;
                    }

                    let mut chunks = match stream.read(true) {
                        Ok(chunks) => chunks,
                        // closed, leave downstream to consume the remaining data
                        // it will then close after checking the flag
                        Err(ReadableError::ClosedStream) => {
                            ctx.closed.store(true, Ordering::Release);
                            break false
                        },
                        Err(ReadableError::IllegalOrderedRead) => {
                            // something strange happened
                            panic!("illegal ordered read")
                        }
                    };

                    let slots = ctx.recv.slots();
                    let mut write_chunk = ctx.recv.write_chunk_uninit(slots).unwrap();
                    let slice = write_chunk.as_mut_slices().0;
                    let mut written = 0;
                    let mut readable = true;
                    for slot in slice {
                        match chunks.next(usize::MAX) {
                            Ok(Some(chunk)) => {
                                slot.write(chunk.bytes);
                                written += 1;
                            },
                            Ok(None) => {
                                ctx.closed.store(true, Ordering::Release);
                                readable = false;
                                break;
                            }
                            /* cannot produce more, remove this stream from the readable list and wait for Readable event */
                            Err(ReadError::Blocked) => {
                                readable = false;
                                break;
                            },
                            Err(ReadError::Reset(err)) => {
                                panic!("stream {} reset: {}", id, err)
                            }
                        }
                    }
                    unsafe { write_chunk.commit(written) };
                    if !readable {
                        break false;
                    }
                }
            });

            writable_streams.retain(|id| {
                let ctx = send_streams.get_mut(id).unwrap();
                let mut stream = conn.send_stream(*id);
                loop {
                    /* if ctx.send is empty, I'm blocked, but this stream is still writable */
                    /* upstream is too slow, wait for it filling at least 25% of the buffer */
                    if ctx.send.is_empty() {
                        // I'll be wakened up by the next poll_write or poll_flush
                        ctx.blocked.store(true, Ordering::Relaxed);
                        // the value of `closed` must be reliable, or I could sleep forever
                        // when the stream is closed, make sure that there's really nothing left
                        /* is_empty is checked after an Acquire load of `closed`,
                        which should be after a Release store of `closed` of upstream,
                        which is in turn after send.push of upstream */
                        if !ctx.closed.load(Ordering::Acquire) || ctx.send.is_empty() {
                            blocked_send_streams.insert(*id);
                            break true;
                        }
                    }

                    let slots = ctx.send.slots();
                    let mut read_chunk = ctx.send.read_chunk(slots).unwrap();
                    let slice = read_chunk.as_mut_slices().0;
                    let expect = slice.len();
                    let written = stream.write_chunks(slice);
                    match written {
                        Ok(written) => {
                            let written = written.chunks;
                            read_chunk.commit(written);
                            /* did I just unblock upstream? test here */
                            if slots - written <= QUIC_STREAM_RECV_UNBLOCK_THRESHOLD {
                                ctx.waker.wake();
                            }
                            /* cannot consume more, remove this stream from the writable list and wait for Writable event */
                            if written < expect {
                                break false;
                            }
                            /* all written, go to the next slice */
                        }
                        /* cannot consume more, remove this stream from the writable list and wait for Writable event */
                        Err(WriteError::Blocked) => break false,
                        Err(WriteError::Stopped(err)) => {
                            panic!("stream {} stopped: {:?}", id, err)
                        }
                        Err(WriteError::ClosedStream) => {
                            ctx.closed.store(true, Ordering::Relaxed);
                            break false; // maybe remove the stream here
                        }
                    }
                }
            });
        }
    }
}
