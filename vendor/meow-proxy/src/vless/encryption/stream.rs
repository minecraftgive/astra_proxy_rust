//! Vision-aware boundary control for the encrypted record relay.
use std::{
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::Notify,
};

#[derive(Default)]
pub(super) struct Control {
    pub read_raw: AtomicBool,
    pub consumed: AtomicU64,
    pub changed: Notify,
    // u64::MAX until Vision has finished its last encrypted write.
    pub write_raw_at: AtomicU64,
}
impl Control {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            write_raw_at: AtomicU64::new(u64::MAX),
            ..Default::default()
        })
    }
}

pub(crate) struct EncryptionStream {
    inner: crate::tasked_duplex::TaskedDuplex,
    control: Arc<Control>,
    pending_read: usize,
    written: u64,
}
impl EncryptionStream {
    pub(super) fn new(inner: crate::tasked_duplex::TaskedDuplex, control: Arc<Control>) -> Self {
        Self {
            inner,
            control,
            pending_read: 0,
            written: 0,
        }
    }
    pub(crate) fn enable_raw_read(&mut self) {
        self.control.read_raw.store(true, Ordering::Release);
    }
    pub(crate) fn enable_raw_write(&mut self) {
        self.control
            .write_raw_at
            .store(self.written, Ordering::Release);
    }
}
impl AsyncRead for EncryptionStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        // Acknowledge the previous bytes only when the consumer asks for more.
        // Vision can inspect DIRECT and switch before the relay reads ahead.
        if self.pending_read != 0 {
            self.control
                .consumed
                .fetch_add(self.pending_read as u64, Ordering::Release);
            self.pending_read = 0;
            self.control.changed.notify_one();
        }
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        self.pending_read = buf.filled().len() - before;
        result
    }
}
impl AsyncWrite for EncryptionStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, data) {
            Poll::Ready(Ok(n)) => {
                self.written += n as u64;
                Poll::Ready(Ok(n))
            }
            result => result,
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
