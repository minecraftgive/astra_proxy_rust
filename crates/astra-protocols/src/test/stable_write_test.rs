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
    stream.write_all(b"defgh").await.unwrap();
    stream.flush().await.unwrap();
    stream.write_all(b"ij").await.unwrap();
    stream.shutdown().await.unwrap();
    assert_eq!(stream.inner.written, b"abcdefghij");
}
