use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// yamux echo server over a duplex pipe (crate-native server mode).
async fn run_echo_server<S>(io: S) -> tokio::task::JoinHandle<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let config = yamux::Config::default();
        let io = io.compat();
        let mut connection = yamux::Connection::new(io, config, yamux::Mode::Server);
        loop {
            match std::future::poll_fn(|cx| connection.poll_next_inbound(cx)).await {
                Some(Ok(stream)) => {
                    tokio::spawn(async move {
                        let mut stream = stream.compat();
                        let mut buf = vec![0u8; 4096];
                        loop {
                            match stream.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if stream.write_all(&buf[..n]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
                Some(Err(_)) | None => return,
            }
        }
    })
}

#[tokio::test]
async fn open_stream_echo_round_trip() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let _server = run_echo_server(server_io).await;
    let session = Arc::new(Session::client(client_io).unwrap());
    let mut stream = session.open_stream().await.unwrap();

    stream.write_all(b"hello yamux").await.unwrap();
    let mut resp = [0u8; 11];
    stream.read_exact(&mut resp).await.unwrap();
    assert_eq!(&resp, b"hello yamux");
}

#[tokio::test]
async fn two_streams_multiplex_independently() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let _server = run_echo_server(server_io).await;
    let session = Arc::new(Session::client(client_io).unwrap());

    let mut a = session.open_stream().await.unwrap();
    let mut b = session.open_stream().await.unwrap();

    a.write_all(b"AAA").await.unwrap();
    b.write_all(b"BBB").await.unwrap();
    let mut buf = [0u8; 3];
    a.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"AAA");
    b.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"BBB");
}

#[tokio::test]
async fn concurrent_opens_all_succeed() {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let _server = run_echo_server(server_io).await;
    let session = Arc::new(Session::client(client_io).unwrap());

    // Open many streams at once: the driver must queue every request
    // instead of dropping earlier ones for later arrivals.
    let opens = (0..8)
        .map(|i| {
            let session = Arc::clone(&session);
            async move { (i, session.open_stream().await.unwrap()) }
        })
        .collect::<Vec<_>>();
    let streams = futures::future::join_all(opens).await;
    assert_eq!(streams.len(), 8);
    for (i, mut stream) in streams {
        let payload = format!("stream-{i}").into_bytes();
        stream.write_all(&payload).await.unwrap();
        let mut resp = vec![0u8; payload.len()];
        stream.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, payload);
    }
}

/// IO wrapper that flags when dropped, so a test can observe the
/// physical connection actually being released (mirrors smux's
/// equivalent test helper).
struct DropTrackedIo {
    inner: tokio::io::DuplexStream,
    dropped: Arc<AtomicBool>,
}

impl Drop for DropTrackedIo {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

impl AsyncRead for DropTrackedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for DropTrackedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Pins issue #427's yamux/h2mux driver-shutdown convergence: dropping
/// the last `Session` reference must release the physical IO promptly
/// (via the new `Drop` impl's `_task.abort()`), the same invariant
/// h2mux::Session and smux::Session already guarantee.
#[tokio::test]
async fn dropping_idle_session_releases_physical_io() {
    let (client_io, _server_io) = tokio::io::duplex(64 * 1024);
    let dropped = Arc::new(AtomicBool::new(false));
    let session = Session::client(DropTrackedIo {
        inner: client_io,
        dropped: Arc::clone(&dropped),
    })
    .unwrap();
    drop(session);

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping an idle session must stop its driver and release the fd");
}
