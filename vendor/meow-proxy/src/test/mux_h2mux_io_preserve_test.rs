use super::*;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::ReadBuf;

/// An inner stream that fails every op with a raw errno (issue #680).
#[derive(Debug)]
struct ErrnoStream(i32);

impl AsyncRead for ErrnoStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::from_raw_os_error(self.0)))
    }
}

impl AsyncWrite for ErrnoStream {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::from_raw_os_error(self.0)))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::from_raw_os_error(self.0)))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::from_raw_os_error(self.0)))
    }
}

#[tokio::test]
async fn session_client_preserves_handshake_errno() {
    let Err(err) = Session::client(ErrnoStream(105)).await else {
        panic!("handshake on an always-failing stream must error");
    };
    assert_eq!(err.raw_os_error(), Some(105));
}
