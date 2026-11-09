use super::*;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::AsyncWrite;

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    limit: usize,
    pending: bool,
    calls: usize,
    flushed: bool,
}

impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.pending {
            this.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let count = data.len().min(this.limit);
        this.bytes.extend_from_slice(&data[..count]);
        this.calls += 1;
        this.pending = true;
        Poll::Ready(Ok(count))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().flushed = true;
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn frame_matches_legacy_wire_with_partial_writes() {
    let message = (API_VERSION, "节点\0name", vec![0u8, 1, 255], true);
    let payload = bincode::serialize(&message).unwrap();
    let mut expected = (payload.len() as u32).to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);
    for chunk in [2, usize::MAX] {
        let mut writer = Writer {
            limit: chunk,
            ..Default::default()
        };
        send_bounded(&mut writer, &message, payload.len() as u32)
            .await
            .unwrap();
        assert_eq!(writer.bytes, expected);
        assert!(!writer.flushed, "bincode framing must not add a flush");
    }
}

#[tokio::test]
async fn rejected_frame_never_writes_a_prefix() {
    let mut writer = Writer {
        limit: usize::MAX,
        ..Default::default()
    };
    let error = send_bounded(&mut writer, &"too large", 1)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(writer.bytes.is_empty());
    assert_eq!(writer.calls, 0);
}

#[tokio::test]
async fn write_zero_is_reported() {
    let mut writer = Writer::default();
    assert_eq!(
        send_management(&mut writer, &42u64)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::WriteZero
    );
}

struct Invalid;
impl Serialize for Invalid {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("Invalid", 2)?;
        value.serialize_field("first", &1u64)?;
        Err(serde::ser::Error::custom("second field cannot serialize"))
    }
}

#[tokio::test]
async fn serialization_failure_never_writes_a_prefix() {
    let mut writer = Writer {
        limit: usize::MAX,
        ..Default::default()
    };
    let error = send_management(&mut writer, &Invalid).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(writer.bytes.is_empty());
}
