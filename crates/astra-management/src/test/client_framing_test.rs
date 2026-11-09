use super::*;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

struct Writer {
    bytes: Vec<u8>,
    chunk: usize,
    pending: bool,
    writes: usize,
    flushes: usize,
    flush_error: bool,
}
impl Writer {
    fn new(chunk: usize) -> Self {
        Self {
            bytes: Vec::new(),
            chunk,
            pending: false,
            writes: 0,
            flushes: 0,
            flush_error: false,
        }
    }
}
impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.pending {
            this.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let size = this.chunk.min(bytes.len());
        this.bytes.extend_from_slice(&bytes[..size]);
        this.writes += 1;
        this.pending = true;
        Poll::Ready(Ok(size))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.flushes += 1;
        Poll::Ready(if this.flush_error {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        })
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn coalesced_json_frame_preserves_bytes_and_flush() {
    let request = Request::new(
        "节点\0id",
        crate::v2::RequestBody::Query(crate::v2::Query::Agents),
    );
    let body = serde_json::to_vec(&request).unwrap();
    let mut expected = b"AMV2".to_vec();
    expected.extend_from_slice(&(body.len() as u32).to_be_bytes());
    expected.extend_from_slice(&body);
    for chunk in [3, usize::MAX] {
        let mut writer = Writer::new(chunk);
        send(&mut writer, &request).await.unwrap();
        assert_eq!(writer.bytes, expected);
        assert_eq!(writer.flushes, 1);
        if chunk == usize::MAX {
            assert_eq!(writer.writes, 1);
        }
        let decoded: Request = receive(&mut writer.bytes.as_slice()).await.unwrap();
        assert_eq!(decoded.id, request.id);
    }
}

#[tokio::test]
async fn oversized_json_is_rejected_before_writing_or_flushing() {
    let mut writer = Writer::new(usize::MAX);
    let error = send(&mut writer, &"x".repeat(MAX_FRAME_BYTES))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exceeds limit"));
    assert!(writer.bytes.is_empty());
    assert_eq!((writer.writes, writer.flushes), (0, 0));
}

#[tokio::test]
async fn invalid_json_map_key_does_not_write_a_partial_frame() {
    let mut writer = Writer::new(usize::MAX);
    let invalid = std::collections::BTreeMap::from([([0u8, 1], "value")]);
    assert!(send(&mut writer, &invalid).await.is_err());
    assert!(writer.bytes.is_empty());
    assert_eq!((writer.writes, writer.flushes), (0, 0));
}

#[tokio::test]
async fn underlying_write_and_flush_errors_are_preserved() {
    let mut stalled = Writer::new(0);
    let error = send(&mut stalled, &"message").await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::WriteZero
    );
    assert_eq!(stalled.flushes, 0);
    let mut failed = Writer::new(usize::MAX);
    failed.flush_error = true;
    let error = send(&mut failed, &"message").await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(failed.flushes, 1);
}
