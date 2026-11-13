//! Per-group epochs are shared across configuration generations. Only connections
//! whose resolved path contains a switched group observe its cancellation.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll, Wake, Waker},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::watch,
};

pub(crate) const COPY_BUFFER_SIZE: usize = 32 * 1024;

#[derive(Default)]
pub(crate) struct Sessions(Mutex<HashMap<String, watch::Sender<u64>>>);
pub(crate) struct Lease(
    Vec<(watch::Receiver<u64>, u64)>,
    pub Option<String>,
    // Keep only cancellation epochs alive after the configuration is released.
    #[allow(dead_code)] Arc<Sessions>,
    pub Vec<String>,
    pub Vec<String>,
);
impl Sessions {
    fn groups(&self) -> std::sync::MutexGuard<'_, HashMap<String, watch::Sender<u64>>> {
        match self.0.lock() {
            Ok(groups) => groups,
            Err(poisoned) => {
                // Epoch updates may have stopped partway through. Closing every
                // sender invalidates the old leases before we accept new ones.
                let mut groups = poisoned.into_inner();
                groups.clear();
                self.0.clear_poison();
                tracing::error!(
                    "proxy group session state panicked; cancelling existing group sessions"
                );
                groups
            }
        }
    }
    pub fn invalidate(&self) {
        self.groups().clear();
    }
    pub fn lease(self: &Arc<Self>, path: &[String]) -> Lease {
        let mut groups = self.groups();
        groups.retain(|_, sender| sender.receiver_count() > 0);
        Lease(
            path.iter()
                .map(|name| {
                    let sender = groups
                        .entry(name.clone())
                        .or_insert_with(|| watch::channel(0).0);
                    let receiver = sender.subscribe();
                    let epoch = *receiver.borrow();
                    (receiver, epoch)
                })
                .collect(),
            None,
            Arc::clone(self),
            Vec::new(),
            path.to_vec(),
        )
    }
    pub fn disconnect(&self, group: &str, reason: &str) {
        let groups = self.groups();
        if let Some(sender) = groups.get(group) {
            sender.send_modify(|v| *v = v.wrapping_add(1));
        }
        tracing::info!(%group, %reason, "closing previous proxy group connections");
    }
}
impl Lease {
    pub fn epochs(&self) -> Vec<(String, u64)> {
        self.4
            .iter()
            .cloned()
            .zip(self.0.iter().map(|(_, epoch)| *epoch))
            .collect()
    }
    pub async fn cancelled(&self) {
        if self.0.is_empty() {
            std::future::pending::<()>().await;
            return;
        }
        let waits: Vec<_> = self
            .0
            .iter()
            .map(|(receiver, epoch)| {
                let mut receiver = receiver.clone();
                let epoch = *epoch;
                Box::pin(async move {
                    loop {
                        if *receiver.borrow_and_update() != epoch {
                            return;
                        }
                        if receiver.changed().await.is_err() {
                            return;
                        }
                    }
                }) as Pin<Box<dyn Future<Output = ()> + Send>>
            })
            .collect();
        futures_util::future::select_all(waits).await;
    }
    pub fn wrap(self, upstream: crate::outbound::Stream) -> crate::outbound::Stream {
        if self.0.is_empty() {
            return upstream;
        }
        Box::new(Stream {
            inner: Some(upstream),
            cancellation: Box::pin(async move { self.cancelled().await }),
            wake: Arc::new(CancellationWake::default()),
        })
    }
}

// A split stream can have different tasks waiting for reads and writes. The
// owned cancellation future must wake both, even though it registers one waker.
#[derive(Default)]
struct CancellationWake {
    read: futures_util::task::AtomicWaker,
    write: futures_util::task::AtomicWaker,
}
impl Wake for CancellationWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.read.wake();
        self.write.wake();
    }
}
struct Stream {
    inner: Option<crate::outbound::Stream>,
    cancellation: Pin<Box<dyn Future<Output = ()> + Send>>,
    wake: Arc<CancellationWake>,
}
impl Stream {
    fn cancelled(&mut self, cx: &Context<'_>, reading: bool) -> bool {
        if self.inner.is_none() {
            return true;
        }
        if reading {
            self.wake.read.register(cx.waker());
        } else {
            self.wake.write.register(cx.waker());
        }
        let waker = Waker::from(Arc::clone(&self.wake));
        if self
            .cancellation
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_ready()
        {
            // Drop the upstream immediately, including when only the read half
            // is polled after cancellation. Never poll a completed future again.
            self.inner = None;
            return true;
        }
        false
    }
    fn broken_pipe() -> io::Error {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "proxy group connection cancelled",
        )
    }
}
impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.cancelled(cx, true) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(this.inner.as_mut().unwrap()).poll_read(cx, buf)
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.cancelled(cx, false) {
            return Poll::Ready(Err(Self::broken_pipe()));
        }
        Pin::new(this.inner.as_mut().unwrap()).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.cancelled(cx, false) {
            return Poll::Ready(Err(Self::broken_pipe()));
        }
        Pin::new(this.inner.as_mut().unwrap()).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.cancelled(cx, false) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(this.inner.as_mut().unwrap()).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "../test/proxy_group_sessions_test.rs"]
mod tests;
