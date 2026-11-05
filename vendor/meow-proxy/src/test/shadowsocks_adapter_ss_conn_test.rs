use super::*;
use shadowsocks::relay::tcprelay::proxy_stream::ProxyServerStream;
use std::io;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::time::{timeout, Instant};

type Cipher = (&'static str, &'static str);
const AEAD: Cipher = ("aes-128-gcm", "server-first-test");
/// 16-byte iPSK ("1234567890123456"), base64.
const AEAD_2022: Cipher = ("2022-blake3-aes-128-gcm", "MTIzNDU2Nzg5MDEyMzQ1Ng==");
const BANNER: &[u8] = b"220 smtp.test ESMTP ready\r\n";
/// Virtual-time bound for an exchange: without a header the client
/// waits forever, and the paused clock jumps straight to this.
const DEADLINE: Duration = Duration::from_secs(5);

fn target() -> Address {
    Address::DomainNameAddress("smtp.test".into(), 25)
}

fn server_config((method, key): Cipher) -> ServerConfig {
    ServerConfig::new(("127.0.0.1", 8388), key, method.parse().unwrap()).unwrap()
}

fn server_stream(cipher: Cipher, io: DuplexStream) -> ProxyServerStream<DuplexStream> {
    let cfg = server_config(cipher);
    ProxyServerStream::from_stream(
        Context::new_shared(ServerType::Server),
        io,
        cfg.method(),
        cfg.key(),
    )
}

/// Transport under the SS codec: records the size of every completed
/// write, i.e. each encrypted chunk (or chunk prefix) put on the wire.
struct Recorder {
    inner: DuplexStream,
    writes: Arc<Mutex<Vec<usize>>>,
}

impl AsyncRead for Recorder {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Recorder {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let res = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = res {
            self.writes.lock().unwrap().push(n);
        }
        res
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct Pair {
    client: SsConn<Recorder>,
    writes: Arc<Mutex<Vec<usize>>>,
    server: ProxyServerStream<DuplexStream>,
}

/// A client `SsConn` and the matching server stream over a duplex pipe
/// holding at most `capacity` bytes per direction.
fn pair(cipher: Cipher, capacity: usize) -> Pair {
    let (c, s) = tokio::io::duplex(capacity);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let transport = Recorder {
        inner: c,
        writes: Arc::clone(&writes),
    };
    let client = SsConn::new(ProxyClientStream::from_stream(
        Context::new_shared(ServerType::Local),
        transport,
        &server_config(cipher),
        target(),
    ));
    Pair {
        client,
        writes,
        server: server_stream(cipher, s),
    }
}

fn recorded(writes: &Mutex<Vec<usize>>) -> Vec<usize> {
    writes.lock().unwrap().clone()
}

/// Poll the client's read side exactly once, like one relay turn.
async fn poll_read_once<R: AsyncRead + Unpin>(conn: &mut R) -> Poll<io::Result<usize>> {
    let mut storage = [0u8; 64];
    std::future::poll_fn(|cx| {
        let mut rb = ReadBuf::new(&mut storage);
        Poll::Ready(
            Pin::new(&mut *conn)
                .poll_read(cx, &mut rb)
                .map_ok(|()| rb.filled().len()),
        )
    })
    .await
}

/// SMTP-like server: greets as soon as the request header names the
/// target, before the client sent a byte, then answers one line.
async fn smtp_server(mut server: ProxyServerStream<DuplexStream>) -> io::Result<()> {
    let addr = server.handshake().await?;
    assert_eq!(addr, target());
    server.write_all(BANNER).await?;
    server.flush().await?;
    let mut line = [0u8; 6];
    server.read_exact(&mut line).await?;
    assert_eq!(&line, b"QUIT\r\n");
    server.write_all(b"221 bye\r\n").await?;
    server.flush().await
}

/// shadowsocks 1.24 draws the AEAD-2022 padding of a payload-less
/// header from `0..=900` (`relay/mod.rs` `get_aead_2022_padding_size`),
/// while its own server rejects a payload-less header with zero padding
/// — 1 in 901 header-only requests fails upstream. Not this adapter's
/// bug: tests re-run that exchange rather than flake.
fn is_zero_padding_reject(e: &io::Error) -> bool {
    e.to_string().contains("padding is 0")
}

/// One server-first exchange through `conn`, which has written nothing
/// yet. `None`: the server hit the upstream zero-padding reject.
async fn banner_exchange<C: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut C,
    server: tokio::task::JoinHandle<io::Result<()>>,
    writes: Option<&Mutex<Vec<usize>>>,
) -> Option<()> {
    let start = Instant::now();
    let mut banner = vec![0u8; BANNER.len()];
    let read = timeout(DEADLINE, conn.read_exact(&mut banner))
        .await
        .expect("banner never arrived: the SS request header was never sent");
    let elapsed = start.elapsed();
    if read.is_err() {
        match server.await.unwrap() {
            Err(e) if is_zero_padding_reject(&e) => return None,
            other => panic!("banner read failed: {read:?}; server: {other:?}"),
        }
    }
    assert_eq!(banner, BANNER);
    assert!(
        elapsed >= SS_HEADER_WINDOW,
        "header must wait out the window for a first payload: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(2), "banner took {elapsed:?}");
    if let Some(w) = writes {
        assert_eq!(recorded(w).len(), 1, "the header goes out as one write");
        // Once the header is out, an empty write is a no-op on the wire.
        assert_eq!(conn.write(&[]).await.unwrap(), 0);
        assert_eq!(
            recorded(w).len(),
            1,
            "an empty write after the header must not emit an empty AEAD chunk"
        );
    }
    conn.write_all(b"QUIT\r\n").await.unwrap();
    conn.flush().await.unwrap();
    let mut bye = [0u8; 9];
    conn.read_exact(&mut bye).await.unwrap();
    assert_eq!(&bye, b"221 bye\r\n");
    if let Some(w) = writes {
        assert_eq!(recorded(w).len(), 2, "QUIT is a chunk of its own");
    }
    server.await.unwrap().unwrap();
    Some(())
}

/// The client never writes before the banner (SMTP/FTP/IMAP/MySQL...):
/// the header goes out alone once the window expires, the banner
/// arrives, and the connection carries data both ways afterwards.
async fn server_first(cipher: Cipher) {
    for _ in 0..3 {
        let Pair {
            mut client,
            writes,
            server,
        } = pair(cipher, 64 * 1024);
        let server = tokio::spawn(smtp_server(server));
        if banner_exchange(&mut client, server, Some(&writes))
            .await
            .is_some()
        {
            return;
        }
    }
    panic!("three zero-padding rejects in a row");
}

#[tokio::test(start_paused = true)]
async fn server_first_banner_arrives_after_window_aead() {
    server_first(AEAD).await;
}

#[tokio::test(start_paused = true)]
async fn server_first_banner_arrives_after_window_aead_2022() {
    server_first(AEAD_2022).await;
}

/// The relay reads the remote side first, which arms the window; a
/// first payload inside it still carries the header in the same single
/// write, and the expired window adds nothing afterwards.
async fn client_first(cipher: Cipher) {
    let Pair {
        mut client,
        writes,
        mut server,
    } = pair(cipher, 64 * 1024);
    const HELLO: &[u8] = b"EHLO client-first\r\n";
    let echo = tokio::spawn(async move {
        assert_eq!(server.handshake().await?, target());
        let mut buf = [0u8; 64];
        let n = server.read(&mut buf).await?;
        server.write_all(&buf[..n]).await?;
        server.flush().await?;
        // Nothing else may arrive before the client's EOF.
        server.read(&mut buf).await
    });

    assert!(poll_read_once(&mut client).await.is_pending());
    tokio::time::advance(SS_HEADER_WINDOW / 2).await;
    client.write_all(HELLO).await.unwrap();
    client.flush().await.unwrap();
    assert_eq!(
        recorded(&writes).len(),
        1,
        "header and first payload must leave in one write"
    );
    let mut back = [0u8; HELLO.len()];
    client.read_exact(&mut back).await.unwrap();
    assert_eq!(back, HELLO);

    tokio::time::advance(SS_HEADER_WINDOW * 5).await;
    assert!(poll_read_once(&mut client).await.is_pending());
    assert_eq!(client.write(&[]).await.unwrap(), 0);
    assert_eq!(
        recorded(&writes).len(),
        1,
        "no header-only or empty chunk after the coalesced first write"
    );
    client.shutdown().await.unwrap();
    assert_eq!(echo.await.unwrap().unwrap(), 0, "server saw extra bytes");
}

#[tokio::test(start_paused = true)]
async fn client_first_payload_coalesces_header_aead() {
    client_first(AEAD).await;
}

#[tokio::test(start_paused = true)]
async fn client_first_payload_coalesces_header_aead_2022() {
    client_first(AEAD_2022).await;
}

/// An explicit empty write still asks for the header (the crate's
/// documented hook), once; a second one must not put an empty AEAD
/// chunk on the wire.
#[tokio::test(start_paused = true)]
async fn explicit_empty_write_sends_header_once() {
    let Pair {
        mut client,
        writes,
        mut server,
    } = pair(AEAD, 64 * 1024);
    assert_eq!(client.write(&[]).await.unwrap(), 0);
    client.flush().await.unwrap();
    assert_eq!(
        recorded(&writes).len(),
        1,
        "empty first write sends the header"
    );
    assert_eq!(client.write(&[]).await.unwrap(), 0);
    client.flush().await.unwrap();
    assert_eq!(
        recorded(&writes).len(),
        1,
        "a second empty write must not emit an empty AEAD chunk"
    );
    assert_eq!(server.handshake().await.unwrap(), target());
}

/// The header-only write stalls on a full pipe and the caller's first
/// payload arrives meanwhile. While stalled the crate ignores the buffer
/// it is handed and would report the payload written — it must instead
/// go out intact after the header.
#[tokio::test(start_paused = true)]
async fn stalled_header_write_keeps_first_payload() {
    // 16 bytes cannot hold even the AEAD salt plus the length chunk.
    let Pair {
        mut client,
        writes,
        mut server,
    } = pair(AEAD, 16);
    assert!(poll_read_once(&mut client).await.is_pending());
    tokio::time::advance(SS_HEADER_WINDOW).await;
    assert!(poll_read_once(&mut client).await.is_pending());
    assert_eq!(
        recorded(&writes),
        [16],
        "the expired window starts the header-only write"
    );

    let srv = tokio::spawn(async move {
        let addr = server.handshake().await?;
        let mut buf = [0u8; 7];
        server.read_exact(&mut buf).await?;
        io::Result::Ok((addr, buf))
    });
    client.write_all(b"payload").await.unwrap();
    client.flush().await.unwrap();
    let (addr, got) = timeout(DEADLINE, srv)
        .await
        .expect("first payload was swallowed by the stalled header write")
        .unwrap()
        .unwrap();
    assert_eq!(addr, target());
    assert_eq!(&got, b"payload");
}

/// A caller that closes without writing sends no header, as before, and
/// the expired window must not write after the shutdown (that would
/// fail the read side with a write error).
#[tokio::test(start_paused = true)]
async fn shutdown_before_first_write_sends_nothing() {
    let Pair {
        mut client,
        writes,
        mut server,
    } = pair(AEAD, 64 * 1024);
    assert!(poll_read_once(&mut client).await.is_pending());
    client.shutdown().await.unwrap();
    tokio::time::advance(SS_HEADER_WINDOW * 2).await;
    assert!(poll_read_once(&mut client).await.is_pending());
    assert!(recorded(&writes).is_empty());
    assert!(server.handshake().await.is_err(), "EOF before any header");
}

/// Relay-chain last hop: `connect_over` must return the same
/// server-first-aware conn as a direct dial.
#[tokio::test(start_paused = true)]
async fn connect_over_sends_header_for_server_first() {
    let adapter = ShadowsocksAdapter::new(
        "ss-test",
        "127.0.0.1",
        8388,
        AEAD.1,
        AEAD.0,
        false,
        None,
        None,
        None,
        Arc::new(crate::dialer::DirectDialer),
    )
    .unwrap();
    let meta = Metadata {
        host: "smtp.test".into(),
        dst_port: 25,
        ..Default::default()
    };
    let (c, s) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(smtp_server(server_stream(AEAD, s)));
    let mut conn = adapter
        .connect_over(Box::new(crate::stream_conn::StreamConn(Box::new(c))), &meta)
        .await
        .unwrap();
    assert!(banner_exchange(&mut conn, server, None).await.is_some());
}
