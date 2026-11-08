//! Bidirectional TCP relay that does not outlive a half-closed peer.
//!
//! `copy_bidirectional` keeps the remaining direction open indefinitely after
//! one side sends FIN. Through proxy protocols the far end often never closes
//! in response, so a client that has already gone (CLOSE_WAIT, then CLOSED)
//! keeps both descriptors until the process runs out of them. Once either
//! direction has finished, the other must keep moving data or the relay ends.
use crate::proxy::group_sessions::COPY_BUFFER_SIZE;
use std::{
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::Notify,
    time::Instant,
};

/// Inactivity allowed on the surviving direction after the other has closed.
pub(crate) const HALF_CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Relay until both directions finish, or until the surviving direction of a
/// half-closed relay is idle for [`HALF_CLOSE_TIMEOUT`]. Returns the bytes
/// copied from `a` to `b` and from `b` to `a`.
pub(crate) async fn relay<A, B>(a: &mut A, b: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    relay_with(a, b, HALF_CLOSE_TIMEOUT).await
}

pub(crate) async fn relay_with<A, B>(
    a: &mut A,
    b: &mut B,
    linger: Duration,
) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let progress = Arc::new(Progress::new());
    let mut a = Tracked {
        inner: a,
        progress: progress.clone(),
    };
    let copy =
        tokio::io::copy_bidirectional_with_sizes(&mut a, b, COPY_BUFFER_SIZE, COPY_BUFFER_SIZE);
    tokio::select! {
        result = copy => result,
        () = progress.linger_expired(linger) => {
            let (up, down) = progress.totals();
            tracing::debug!(up, down, linger_secs = linger.as_secs(),
                "closing half-closed relay after inactivity");
            Ok((up, down))
        }
    }
}

/// Every byte in either direction passes through `a`, so observing it alone
/// sees all activity and both ways a direction can end: EOF read from `a`, or
/// `a` shut down after EOF from `b`.
struct Progress {
    origin: Instant,
    last_nanos: AtomicU64,
    up: AtomicU64,
    down: AtomicU64,
    half_closed: AtomicBool,
    closed: Notify,
}

impl Progress {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            last_nanos: AtomicU64::new(0),
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
            half_closed: AtomicBool::new(false),
            closed: Notify::new(),
        }
    }
    fn touch(&self) {
        let nanos = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.last_nanos.store(nanos, Ordering::Relaxed);
    }
    fn last(&self) -> Instant {
        self.origin + Duration::from_nanos(self.last_nanos.load(Ordering::Relaxed))
    }
    fn half_close(&self) {
        self.touch();
        if !self.half_closed.swap(true, Ordering::Relaxed) {
            self.closed.notify_one();
        }
    }
    fn totals(&self) -> (u64, u64) {
        (
            self.up.load(Ordering::Relaxed),
            self.down.load(Ordering::Relaxed),
        )
    }
    async fn linger_expired(&self, linger: Duration) {
        // notify_one stores a permit, so a half-close before this await is seen.
        self.closed.notified().await;
        loop {
            let deadline = self.last() + linger;
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

struct Tracked<'a, S: ?Sized> {
    inner: &'a mut S,
    progress: Arc<Progress>,
}

impl<S: AsyncRead + Unpin + ?Sized> AsyncRead for Tracked<'_, S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let wanted = buf.remaining() > 0;
        let result = Pin::new(&mut *this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            let read = buf.filled().len() - before;
            if read > 0 {
                this.progress.up.fetch_add(read as u64, Ordering::Relaxed);
                this.progress.touch();
            } else if wanted {
                this.progress.half_close();
            }
        }
        result
    }
}

impl<S: AsyncWrite + Unpin + ?Sized> AsyncWrite for Tracked<'_, S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut *this.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(written)) = result {
            if written > 0 {
                this.progress
                    .down
                    .fetch_add(written as u64, Ordering::Relaxed);
                this.progress.touch();
            }
        }
        result
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut *this.inner).poll_shutdown(cx);
        if result.is_ready() {
            this.progress.half_close();
        }
        result
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut *this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(written)) = result {
            if written > 0 {
                this.progress
                    .down
                    .fetch_add(written as u64, Ordering::Relaxed);
                this.progress.touch();
            }
        }
        result
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
#[path = "../test/proxy_relay_test.rs"]
mod tests;
