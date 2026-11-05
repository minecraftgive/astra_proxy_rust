//! h2mux protocol (mihomo default), client side.
//!
//! Mirrors metacubex/sing-mux h2mux.go: the physical connection runs an
//! HTTP/2 client after the mux request header; each logical stream is one
//! CONNECT request to `https://localhost`.  The request body carries
//! client→server bytes as h2 DATA frames, the response body carries
//! server→client bytes.  No gun/grpc framing — raw passthrough.
//!
//! The response is resolved lazily on the first read, mirroring sing-mux's
//! lateHTTPConn: sing-box's h2mux server defers flushing the 200 headers
//! until its first response-body write, so awaiting the response before
//! returning the stream would deadlock (the server waits for our data
//! before it ever writes).

use bytes::Bytes;
use http::{Method, Request, StatusCode, Version};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

pub use meow_transport::h2_common::H2Stream as Stream;
use meow_transport::h2_common::{RecvState, StatusPolicy};

/// Timeout for the CONNECT response; mirrors sing-mux's `TCPTimeout`
/// (5 s).  The deadline is armed when the stream is opened, but it is
/// only *checked* while a read is being polled (`RecvState` resolves the
/// response lazily on the first read), so a stream that is never read
/// never times out.  On expiry the read side fails with TimedOut (the
/// caller tears the stream down).
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for acquiring send readiness (h2 `poll_ready`).  Without this,
/// a peer whose `SETTINGS_MAX_CONCURRENT_STREAMS` is exhausted can park
/// `open_stream` indefinitely — `RESPONSE_TIMEOUT` only covers the
/// response, not the open.  Mirrors `SESSION_SETUP_TIMEOUT` (5 s).
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Surface an io-carrying `h2::Error` as the inner `io::Error` verbatim —
/// `io::Error::other` would erase its `raw_os_error`, which
/// `DialFailureTracker`'s local-resource classification needs
/// (issue #680).
fn h2_to_io(e: h2::Error) -> io::Error {
    if e.is_io() {
        if let Some(io) = e.into_io() {
            return io;
        }
        return io::Error::other("h2mux io error");
    }
    io::Error::other(e)
}

/// h2mux session over one physical connection (client role).  The concrete
/// IO type is erased — the driver task owns the h2 connection.
pub struct Session {
    send_request: h2::client::SendRequest<Bytes>,
    dead: Arc<AtomicBool>,
    _task: tokio::task::JoinHandle<()>,
}

impl Session {
    pub async fn client<S>(io: S) -> io::Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // Proxy-sized receive windows (4 MiB / stream, 16 MiB / connection)
        // instead of h2's 64 KiB default — see
        // `meow_transport::h2_common::client_builder`.
        let (send_request, connection) = meow_transport::h2_common::client_builder()
            .handshake::<_, Bytes>(io)
            .await
            .map_err(h2_to_io)?;
        let dead = Arc::new(AtomicBool::new(false));
        let driver_dead = Arc::clone(&dead);
        // Drive SETTINGS / WINDOW_UPDATE / PING frames; the future resolves
        // when the physical connection dies.
        let task = tokio::spawn(async move {
            let _ = connection.await;
            driver_dead.store(true, Ordering::SeqCst);
        });
        Ok(Self {
            send_request,
            dead,
            _task: task,
        })
    }

    /// Open one h2mux stream: a CONNECT request whose body/response body
    /// form the duplex channel.  Returns immediately — the server may not
    /// flush its 200 until the first response-body write (see module docs).
    pub async fn open_stream(&self) -> io::Result<Stream> {
        let request = Request::builder()
            .method(Method::CONNECT)
            .uri("https://localhost")
            .version(Version::HTTP_2)
            .body(())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut send_request = self.send_request.clone();
        // h2 requires poll_ready/ready before send_request: sending
        // without readiness violates the documented contract and is
        // rejected once MAX_CONCURRENT_STREAMS is exhausted.  A timeout
        // prevents an exhausted peer from parking open_stream forever.
        send_request = tokio::time::timeout(OPEN_TIMEOUT, send_request.ready())
            .await
            .map_err(|_| io::Error::other("h2mux: timed out waiting for send readiness (open)"))?
            .map_err(h2_to_io)?;
        let (response_future, send_stream) = send_request
            .send_request(request, false)
            .map_err(h2_to_io)?;
        Ok(Stream::new(
            send_stream,
            RecvState::with_timeout(
                response_future,
                RESPONSE_TIMEOUT,
                StatusPolicy::Exact(StatusCode::OK),
                "h2mux",
            ),
        )
        .with_remote_no_error_eof())
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// Tear the session down now even while streams still hold it: aborts
    /// the driver (dropping the h2 connection, so every stream fails) and
    /// marks it dead — an aborted driver never reaches its own
    /// `dead.store`. Used by pool resets (issue #695). Idempotent.
    pub fn close(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self._task.abort();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Aborting the driver task drops the h2 connection and the
        // underlying socket immediately.  Without this, an evicted idle
        // session could keep the physical connection (and its fd) alive
        // until the peer closes it.
        self._task.abort();
    }
}

#[cfg(test)]
#[path = "../test/mux_h2mux_test.rs"]
mod tests;

#[cfg(test)]
#[path = "../test/mux_h2mux_io_preserve_test.rs"]
mod io_preserve_tests;
