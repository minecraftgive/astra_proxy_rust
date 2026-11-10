use super::*;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

#[tokio::test(start_paused = true)]
async fn twenty_load_idle_cycles_release_storage_and_regrow() {
    let mut buffer = Buffer::new(32768);
    let (_peer, mut silent) = duplex(32);
    for _ in 0..20 {
        while buffer.len() < 32768 {
            for _ in 0..4 {
                buffer.completed(buffer.len());
            }
        }
        assert_eq!(buffer.len(), 32768);
        assert!(
            tokio::time::timeout(Duration::from_secs(31), buffer.read(&mut silent))
                .await
                .is_err()
        );
        assert_eq!(buffer.len(), 4096);
        for _ in 0..8 {
            buffer.completed(4096);
            tokio::time::advance(Duration::from_millis(101)).await;
        }
        assert_eq!(buffer.len(), 4096, "sparse full transfers must not grow");
    }
}

#[tokio::test]
async fn preserves_payload_under_backpressure_and_independent_half_close() {
    let (mut client, mut a) = duplex(83);
    let (mut b, mut server) = duplex(113);
    let task =
        tokio::spawn(async move { copy_bidirectional(&mut a, &mut b, 32768).await.unwrap() });
    let payload: Vec<_> = (0..300_000).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();
    let peer = tokio::spawn(async move {
        let mut received = Vec::new();
        server.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, expected);
        server.write_all(b"final response after FIN").await.unwrap();
        server.shutdown().await.unwrap();
    });
    client.write_all(&payload).await.unwrap();
    client.shutdown().await.unwrap();
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).await.unwrap();
    assert_eq!(reply, b"final response after FIN");
    peer.await.unwrap();
    assert_eq!(
        task.await.unwrap(),
        (payload.len() as u64, reply.len() as u64)
    );
}

#[tokio::test(start_paused = true)]
async fn pending_bytes_survive_idle_deadline_and_cancellation_releases_owners() {
    let (mut client, mut a) = duplex(65536);
    let (mut b, mut server) = duplex(1);
    let task = tokio::spawn(async move { copy_bidirectional(&mut a, &mut b, 32768).await });
    client.write_all(&vec![42; 40000]).await.unwrap();
    client.shutdown().await.unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    let mut bytes = Vec::new();
    server.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes, vec![42; 40000]);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let mut end = [0; 1];
    assert_eq!(client.read(&mut end).await.unwrap(), 0);
}

struct InterruptedReader {
    inner: std::io::Cursor<Vec<u8>>,
    interrupted: bool,
}
impl AsyncRead for InterruptedReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !std::mem::replace(&mut this.interrupted, true) {
            return Poll::Ready(Err(io::ErrorKind::Interrupted.into()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}
#[derive(Default)]
struct PartialWriter {
    bytes: Vec<u8>,
    interrupted: bool,
    shutdown_polls: usize,
}
impl AsyncWrite for PartialWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !std::mem::replace(&mut this.interrupted, true) {
            return Poll::Ready(Err(io::ErrorKind::Interrupted.into()));
        }
        let n = bytes.len().min(7);
        this.bytes.extend_from_slice(&bytes[..n]);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        assert_eq!(self.shutdown_polls, 0, "flush after shutdown started");
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.shutdown_polls += 1;
        if this.shutdown_polls == 1 {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

#[tokio::test]
async fn retries_interrupted_io_and_resumes_shutdown_without_flushing_again() {
    let expected: Vec<_> = (0..100_000).map(|i| (i % 251) as u8).collect();
    let mut reader = InterruptedReader {
        inner: std::io::Cursor::new(expected.clone()),
        interrupted: false,
    };
    let mut writer = PartialWriter::default();
    let mut direction = Direction::new(32768);
    let copied = poll_fn(|cx| direction.poll(cx, &mut reader, &mut writer))
        .await
        .unwrap();
    assert_eq!(copied, expected.len() as u64);
    assert_eq!(writer.bytes, expected);
    assert_eq!(writer.shutdown_polls, 2);
    assert!(
        direction.buffer.is_empty(),
        "completed direction releases storage"
    );
}

#[tokio::test]
async fn flushes_buffered_writers_while_both_readers_remain_open() {
    let (mut client, a) = duplex(1024);
    let (b, mut server) = duplex(1024);
    let task = tokio::spawn(async move {
        let mut a = tokio::io::BufStream::new(a);
        let mut b = tokio::io::BufStream::new(b);
        copy_bidirectional(&mut a, &mut b, 32768).await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        client.write_all(b"ping").await.unwrap();
        let mut bytes = [0; 4];
        server.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"ping");
        server.write_all(b"pong").await.unwrap();
        client.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"pong");
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap().unwrap(), (4, 4));
    })
    .await
    .unwrap();
}
