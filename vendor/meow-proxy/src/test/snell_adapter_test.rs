use super::*;
use std::future::Future;
use std::time::Duration;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use super::super::protocol::{COMMAND_CONNECT, HEADER_VERSION, RESPONSE_TUNNEL};
use super::super::v3::V3Conn;

struct TestProxyConn(DuplexStream);

impl AsyncRead for TestProxyConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TestProxyConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl ProxyConn for TestProxyConn {}

async fn within<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("test future timed out")
}

#[tokio::test]
async fn connect_over_writes_v3_connect_header_on_existing_stream() {
    let adapter = SnellAdapter::new(
        "snell",
        "unused.invalid",
        8443,
        "test-psk",
        SnellObfs::None,
        SnellVersion::V3,
        false,
        false,
        Arc::new(crate::dialer::DirectDialer),
    )
    .unwrap();
    let metadata = Metadata {
        host: "example.com".into(),
        dst_port: 443,
        ..Metadata::default()
    };
    let (client, server) = tokio::io::duplex(1 << 16);

    let client_fut = adapter.connect_over(Box::new(TestProxyConn(client)), &metadata);
    let server_fut = async {
        let mut server = V3Conn::new(server, Arc::from(b"test-psk".as_slice()));

        let mut got = [0u8; 17];
        server.read_exact(&mut got).await.unwrap();
        let mut expected = vec![HEADER_VERSION, COMMAND_CONNECT, 0, 11];
        expected.extend_from_slice(b"example.com");
        expected.extend_from_slice(&443u16.to_be_bytes());
        assert_eq!(&got[..], &expected[..]);

        server.write_all(&[RESPONSE_TUNNEL]).await.unwrap();
        server.write_all(b"ok").await.unwrap();
        server.flush().await.unwrap();
    };

    let (conn, ()) = within(async { tokio::join!(client_fut, server_fut) }).await;
    let mut conn = conn.unwrap();
    let mut body = [0u8; 2];
    within(conn.read_exact(&mut body)).await.unwrap();
    assert_eq!(&body, b"ok");
}

/// Every Snell server version tears the session down on transport EOF,
/// so a half-close must stop at the zero chunk: the upstream's late
/// reply still has to reach the client.
#[tokio::test]
async fn half_close_keeps_the_transport_open_for_the_reply() {
    use super::super::v4::V4Conn;
    use super::super::v6::V6Conn;

    let psk: Arc<[u8]> = Arc::from(b"test-psk".as_slice());
    let (client, server) = tokio::io::duplex(1 << 16);
    let peer = V3Conn::new(server, Arc::clone(&psk));
    half_close_then_late_reply(SnellVersion::V3, client, peer).await;
    let (client, server) = tokio::io::duplex(1 << 16);
    let peer = V4Conn::new(server, Arc::clone(&psk));
    half_close_then_late_reply(SnellVersion::V4, client, peer).await;
    let (client, server) = tokio::io::duplex(1 << 16);
    let codec = V6Codec::new(SnellV6Mode::Default, b"test-psk");
    let peer = V6Conn::new(server, psk, codec);
    half_close_then_late_reply(SnellVersion::V6, client, peer).await;
}

/// Half-close a `version` session to `peer` (the server side of
/// `client`), check that only the zero chunk crossed, then have the peer
/// reply.
async fn half_close_then_late_reply<P: AsyncRead + AsyncWrite + Unpin>(
    version: SnellVersion,
    client: DuplexStream,
    mut peer: P,
) {
    use super::super::protocol::is_zero_chunk;
    use std::task::Waker;

    let adapter = SnellAdapter::new(
        "snell",
        "unused.invalid",
        8443,
        "test-psk",
        SnellObfs::None,
        version,
        false,
        false,
        Arc::new(crate::dialer::DirectDialer),
    )
    .unwrap();
    let metadata = Metadata {
        host: "example.com".into(),
        dst_port: 443,
        ..Metadata::default()
    };
    let mut conn = within(adapter.connect_over(Box::new(TestProxyConn(client)), &metadata))
        .await
        .unwrap();
    within(conn.write_all(b"ping")).await.unwrap();
    within(conn.shutdown()).await.unwrap();

    // v6 always sends the reuse-capable request.
    let mut expected = connect_request("example.com", 443, version == SnellVersion::V6).unwrap();
    expected.extend_from_slice(b"ping");
    let mut got = vec![0u8; expected.len()];
    within(peer.read_exact(&mut got)).await.unwrap();
    assert_eq!(got, expected, "{version:?}");
    let err = within(peer.read(&mut [0u8; 1])).await.unwrap_err();
    assert!(
        is_zero_chunk(&err),
        "{version:?}: expected the zero chunk, got {err}"
    );
    let mut probe = [0u8; 1];
    let polled = Pin::new(&mut peer).poll_read(
        &mut Context::from_waker(Waker::noop()),
        &mut ReadBuf::new(&mut probe),
    );
    assert!(
        polled.is_pending(),
        "{version:?}: half-close sent a transport FIN"
    );

    within(peer.write_all(&[RESPONSE_TUNNEL])).await.unwrap();
    within(peer.write_all(b"late")).await.unwrap();
    within(peer.write(&[])).await.unwrap();
    within(peer.flush()).await.unwrap();
    let mut reply = Vec::new();
    within(conn.read_to_end(&mut reply)).await.unwrap();
    assert_eq!(reply, b"late", "{version:?}");
}
