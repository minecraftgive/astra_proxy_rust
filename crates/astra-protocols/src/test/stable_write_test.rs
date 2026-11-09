use super::*;
use std::collections::VecDeque;
use tokio::io::AsyncWriteExt;

/// Mimics meow's GunStream: the first write of a frame returns Pending, and a
/// retry with different bytes is an error.
#[derive(Default)]
struct Strict {
    stashed: Option<Vec<u8>>,
    pendings: VecDeque<bool>,
    written: Vec<u8>,
}
impl AsyncWrite for Strict {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(stashed) = &this.stashed {
            if stashed != buf {
                return Poll::Ready(Err(io::Error::other("write buffer changed after Pending")));
            }
        } else {
            this.stashed = Some(buf.to_vec());
        }
        if this.pendings.pop_front().unwrap_or(false) {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        this.written.extend(this.stashed.take().unwrap());
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn pending_bytes_are_replayed_unchanged_before_new_data() {
    let strict = Strict {
        pendings: [true, true, false, true, false].into(),
        ..Default::default()
    };
    let mut stream = StableWrite::new(strict);
    // The second call presents a longer buffer, as tokio's copy may after Pending.
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(matches!(
        Pin::new(&mut stream).poll_write(&mut cx, b"abc"),
        Poll::Ready(Ok(3))
    ));
    assert!(stream.pending.capacity() >= 3);
    stream.write_all(b"defgh").await.unwrap();
    stream.flush().await.unwrap();
    assert_eq!(stream.pending.capacity(), 0);
    stream.write_all(b"ij").await.unwrap();
    stream.shutdown().await.unwrap();
    assert_eq!(stream.inner.written, b"abcdefghij");
    assert_eq!(stream.pending.capacity(), 0);
}

enum Step {
    Pending,
    Write(usize),
    Fail(io::ErrorKind),
}

#[derive(Default)]
struct Partial {
    steps: VecDeque<Step>,
    retry: Option<Vec<u8>>,
    written: Vec<u8>,
    flushed: bool,
    shutdown: bool,
}

impl AsyncWrite for Partial {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(retry) = this.retry.take() {
            assert_eq!(retry, buf, "a pending suffix must be replayed unchanged");
        }
        match this.steps.pop_front().unwrap_or(Step::Write(buf.len())) {
            Step::Pending => {
                this.retry = Some(buf.to_vec());
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Step::Write(limit) => {
                let count = limit.min(buf.len());
                this.written.extend_from_slice(&buf[..count]);
                Poll::Ready(Ok(count))
            }
            Step::Fail(kind) => Poll::Ready(Err(kind.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        assert!(this.retry.is_none());
        this.flushed = true;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        assert!(this.retry.is_none());
        this.shutdown = true;
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn partial_writes_and_pending_preserve_suffix_before_new_data() {
    let inner = Partial {
        steps: [
            Step::Pending,
            Step::Write(2),
            Step::Pending,
            Step::Write(1),
            Step::Pending,
            Step::Write(32),
            Step::Pending,
            Step::Write(32),
        ]
        .into(),
        ..Default::default()
    };
    let mut stream = StableWrite::new(inner);
    stream.write_all(b"abcdef").await.unwrap();
    stream.write_all(b"xyz").await.unwrap();
    stream.flush().await.unwrap();
    assert_eq!(stream.inner.written, b"abcdefxyz");
    assert!(stream.inner.flushed);
    assert_eq!(stream.pending.capacity(), 0);
    stream.shutdown().await.unwrap();
    assert!(stream.inner.shutdown);
}

#[tokio::test]
async fn shutdown_drains_partial_pending_output() {
    let mut stream = StableWrite::new(Partial {
        steps: [Step::Pending, Step::Write(1), Step::Pending, Step::Write(8)].into(),
        ..Default::default()
    });
    stream.write_all(b"finish").await.unwrap();
    stream.shutdown().await.unwrap();
    assert_eq!(stream.inner.written, b"finish");
    assert!(stream.inner.shutdown);
    assert_eq!(stream.pending.capacity(), 0);
}

#[tokio::test]
async fn stalled_or_failed_pending_output_does_not_accept_new_data() {
    for failure in [Step::Write(0), Step::Fail(io::ErrorKind::BrokenPipe)] {
        let expected = match failure {
            Step::Write(_) => io::ErrorKind::WriteZero,
            Step::Fail(kind) => kind,
            Step::Pending => unreachable!(),
        };
        let mut stream = StableWrite::new(Partial {
            steps: [Step::Pending, Step::Write(2), failure].into(),
            ..Default::default()
        });
        stream.write_all(b"original").await.unwrap();
        assert_eq!(stream.write_all(b"new").await.unwrap_err().kind(), expected);
        assert_eq!(stream.inner.written, b"or");
        assert!(!stream.inner.flushed);
        assert!(!stream.inner.shutdown);
    }
}
