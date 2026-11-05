//! yamux protocol (hashicorp/go-yamux wire format), client side, via the
//! libp2p `yamux` crate which interoperates with hashicorp yamux on the
//! wire.
//!
//! yamux 0.14 has no split Control: the `Connection` state machine must be
//! polled continuously to drive reads (data/FIN delivery to streams) AND to
//! open outbound streams.  One driver task owns the connection and polls a
//! hand-rolled state machine each wake-up: drain queued open requests, serve
//! one `poll_new_outbound`, drive one `poll_next_inbound`, then park (all
//! wakers — request channel, stream-open ack, socket — stay registered).
//! The crate speaks the `futures` IO traits; tokio-util compat bridges to
//! tokio's AsyncRead/AsyncWrite.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

/// tokio io bridged to the futures IO traits the yamux crate consumes
/// (\`TokioAsyncReadCompatExt::compat\`).
type FutIo<S> = tokio_util::compat::Compat<S>;

struct OpenRequest {
    respond: oneshot::Sender<std::result::Result<yamux::Stream, yamux::ConnectionError>>,
}

/// yamux session over one physical connection (client role).  The
/// concrete IO type is erased — only the driver task owns the connection.
pub struct Session {
    open_tx: mpsc::UnboundedSender<OpenRequest>,
    dead: Arc<AtomicBool>,
    _task: tokio::task::JoinHandle<()>,
}

impl Session {
    pub fn client<S>(io: S) -> io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // yamux 0.14 dropped the per-stream open/close timeout knobs;
        // sing-mux sets both to 5 s — acceptable divergence.
        let config = yamux::Config::default();
        let io: FutIo<S> = io.compat();
        let mut connection = yamux::Connection::new(io, config, yamux::Mode::Client);
        let (open_tx, mut open_rx) = mpsc::unbounded_channel::<OpenRequest>();
        let dead = Arc::new(AtomicBool::new(false));
        let driver_dead = Arc::clone(&dead);
        let task = tokio::spawn(async move {
            // Queued open requests wait here in FIFO order; every request
            // keeps its own oneshot so none can be dropped by a newer one.
            let mut pending_opens: VecDeque<OpenRequest> = VecDeque::new();
            std::future::poll_fn(|cx| {
                loop {
                    // Drain queued open requests.
                    while let Poll::Ready(item) = open_rx.poll_recv(cx) {
                        match item {
                            Some(request) => pending_opens.push_back(request),
                            // All callers gone: shut the driver down.
                            None => return Poll::Ready(()),
                        }
                    }
                    // Serve stream opens until the connection reports
                    // Pending; looping here keeps the queue moving without
                    // waiting for an external wake-up.
                    if !pending_opens.is_empty() {
                        match connection.poll_new_outbound(cx) {
                            Poll::Ready(result) => {
                                let request = pending_opens.pop_front().expect("checked non-empty");
                                let _ = request.respond.send(result);
                                continue;
                            }
                            Poll::Pending => {}
                        }
                    }
                    // Drive inbound: data/FIN delivery plus connection teardown.
                    match connection.poll_next_inbound(cx) {
                        // Server-initiated streams are not expected in proxy
                        // mux; drop them (reset) and keep driving.
                        Poll::Ready(Some(Ok(stream))) => {
                            tracing::debug!("yamux: server-initiated stream dropped (reset)");
                            drop(stream);
                            continue;
                        }
                        Poll::Ready(Some(Err(_))) | Poll::Ready(None) => return Poll::Ready(()),
                        Poll::Pending => {}
                    }
                    return Poll::Pending;
                }
            })
            .await;
            driver_dead.store(true, Ordering::SeqCst);
        });
        Ok(Self {
            open_tx,
            dead,
            _task: task,
        })
    }

    pub async fn open_stream(&self) -> io::Result<Stream> {
        let (respond, response) = oneshot::channel();
        self.open_tx
            .send(OpenRequest { respond })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "yamux driver gone"))?;
        let stream = response
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "yamux driver dropped request"))?
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, format!("yamux open: {e}")))?;
        Ok(Stream::from(stream))
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// Tear the session down now even while streams still hold it: aborts
    /// the driver (dropping the physical connection, so every stream fails)
    /// and marks it dead — an aborted driver never reaches its own
    /// `dead.store`. Used by pool resets (issue #695). Idempotent.
    pub fn close(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self._task.abort();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Converges with h2mux::Session / smux::Session: aborting the
        // driver task drops the physical connection (and its fd)
        // immediately, instead of relying on `open_tx` closing to end the
        // driver loop. That path works today (MuxStreamKind::Yamux holds a
        // bare yamux::Stream with no back-reference to the Session, so
        // dropping the last stream does not itself end the driver), but an
        // evicted zero-stream session should not be able to keep the
        // socket alive waiting on the peer.
        self._task.abort();
    }
}

/// One yamux stream, exposed with tokio IO traits.
pub struct Stream {
    inner: tokio_util::compat::Compat<yamux::Stream>,
}

impl From<yamux::Stream> for Stream {
    fn from(stream: yamux::Stream) -> Self {
        Self {
            inner: stream.compat(),
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "../test/mux_yamux_test.rs"]
mod tests;
