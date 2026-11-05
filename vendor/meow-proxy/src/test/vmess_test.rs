use super::*;
use crate::dialer::DirectDialer;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

/// Minimal AsyncRead+AsyncWrite+ProxyConn newtype over a duplex half —
/// stands in for the relay chain's upstream leg. Same pattern as
/// `mux::muxcool`'s TestConn.
struct DuplexConn(DuplexStream);

impl ProxyConn for DuplexConn {}

impl AsyncRead for DuplexConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for DuplexConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// `connect_over` must run the VMess request exchange on the supplied
/// stream: the peer side sees a sealed AEAD header that opens to the
/// requested destination — proving the relay path skipped only the raw
/// socket dial, not the protocol handshake.
#[tokio::test]
async fn connect_over_writes_vmess_request_for_metadata() {
    const UUID: [u8; 16] = [
        0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08,
        0x11,
    ];
    let adapter = VmessAdapter::new(
        "vmess-relay-hop",
        "127.0.0.1",
        10086, // unused — connect_over never dials
        UUID,
        Security::Aes128Gcm,
        false,
        TransportChain::empty(),
        Arc::new(DirectDialer),
    );

    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let metadata = Metadata {
        host: "example.com".into(),
        dst_port: 443,
        ..Default::default()
    };
    let _conn = adapter
        .connect_over(Box::new(DuplexConn(client)), &metadata)
        .await
        .expect("connect_over must succeed over duplex");

    // Server side: read the fixed 42-byte prefix, derive the exact
    // frame length from its sealed length block, then read the rest —
    // the header must be consumed exactly, with nothing trailing.
    let mut prefix = [0u8; 42];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut prefix),
    )
    .await
    .expect("sealed header prefix must arrive")
    .expect("sealed header prefix");
    let frame_len = header::tests::request_header_frame_len(&adapter.cmd_key, &prefix)
        .expect("sealed length block must open");
    let mut frame = prefix.to_vec();
    frame.resize(frame_len, 0);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::io::AsyncReadExt::read_exact(&mut server, &mut frame[42..]),
    )
    .await
    .expect("sealed payload must arrive")
    .expect("read");
    let pt = header::tests::server_open_request_header(&adapter.cmd_key, &frame)
        .expect("server must be able to open the sealed header");

    // Plaintext: ver(1)|req_iv(16)|req_key(16)|resp_v(1)|opt(1)|
    //            p+sec(1)|reserved(1)|cmd(1)|port(2)|atyp+addr|pad|fnv(4)
    assert_eq!(pt[37], 0x01, "cmd must be TCP CONNECT");
    let port = u16::from_be_bytes([pt[38], pt[39]]);
    assert_eq!(port, 443, "header must carry metadata.dst_port");
    assert_eq!(pt[40], 0x02, "host must encode as domain (atyp 0x02)");
    let dom_len = pt[41] as usize;
    assert_eq!(
        &pt[42..42 + dom_len],
        b"example.com",
        "header must carry metadata.host"
    );
}

/// `connect_over` must also carry the full duplex exchange, not just the
/// request header: the peer answers the sealed response header, reads
/// AEAD body records the client encrypts, and its encrypted reply
/// records surface as plaintext on the returned conn.
#[tokio::test]
async fn connect_over_carries_full_duplex_vmess_exchange() {
    const UUID: [u8; 16] = [
        0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08,
        0x11,
    ];
    let adapter = VmessAdapter::new(
        "vmess-relay-hop",
        "127.0.0.1",
        10086,
        UUID,
        Security::Aes128Gcm,
        false,
        TransportChain::empty(),
        Arc::new(DirectDialer),
    );

    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let metadata = Metadata {
        host: "example.com".into(),
        dst_port: 443,
        ..Default::default()
    };
    let mut conn = adapter
        .connect_over(Box::new(DuplexConn(client)), &metadata)
        .await
        .expect("connect_over must succeed over duplex");

    // Server side: open the request header, extract the per-connection
    // key material, answer the response header, then echo body records.
    let server_task = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut prefix = [0u8; 42];
        server.read_exact(&mut prefix).await.expect("header prefix");
        let frame_len = header::tests::request_header_frame_len(&adapter.cmd_key, &prefix)
            .expect("sealed length block must open");
        let mut frame = prefix.to_vec();
        frame.resize(frame_len, 0);
        server
            .read_exact(&mut frame[42..])
            .await
            .expect("header payload");
        let pt = header::tests::server_open_request_header(&adapter.cmd_key, &frame)
            .expect("sealed request header must open");

        let req_iv: [u8; 16] = pt[1..17].try_into().unwrap();
        let req_key: [u8; 16] = pt[17..33].try_into().unwrap();
        let resp_v = pt[33];

        server
            .write_all(&header::tests::seal_response_header(
                &req_key, &req_iv, resp_v,
            ))
            .await
            .expect("response header write");

        let mut cipher = body::BodyCipher::server_mirror(Security::Aes128Gcm, &req_key, &req_iv);
        while let Some(rec) = cipher
            .read_record(&mut server)
            .await
            .expect("body record must decrypt")
        {
            cipher
                .write_record(&mut server, &rec)
                .await
                .expect("echo record must encrypt");
        }
    });

    // Client side: two plaintext ping-pongs through the record layer.
    // Bounded: a regression that drops the request header or corrupts
    // framing must fail fast, not park the test.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for payload in [b"ping-one".as_slice(), b"ping-two-longer".as_slice()] {
            conn.write_all(payload).await.expect("write failed");
            conn.flush().await.expect("flush failed");
            let mut buf = vec![0u8; payload.len()];
            tokio::io::AsyncReadExt::read_exact(&mut conn, &mut buf)
                .await
                .expect("echo read failed");
            assert_eq!(&buf, payload, "duplex echo mismatch");
        }
    })
    .await
    .expect("duplex echo timed out");

    drop(conn);
    tokio::time::timeout(std::time::Duration::from_secs(5), server_task)
        .await
        .expect("server echo task timed out")
        .expect("server echo task panicked");
}

#[tokio::test]
async fn support_udp_reports_implemented_udp() {
    const UUID: [u8; 16] = [1; 16];
    let adapter = VmessAdapter::new(
        "vmess-native-udp",
        "127.0.0.1",
        10086,
        UUID,
        Security::Aes128Gcm,
        true,
        TransportChain::empty(),
        Arc::new(DirectDialer),
    );
    assert!(adapter.support_udp());
    let adapter = VmessAdapter::new(
        "vmess-udp-off",
        "127.0.0.1",
        10086,
        UUID,
        Security::Aes128Gcm,
        false,
        TransportChain::empty(),
        Arc::new(DirectDialer),
    );
    assert!(!adapter.support_udp());
    let Err(e) = adapter.dial_udp(&Metadata::default()).await else {
        panic!("plain vmess UDP dial must fail closed");
    };
    assert!(
        !e.to_string().contains("`udp: true`"),
        "unconfigured adapter keeps the plain message: {e}"
    );
}

/// UDP-capable mux works without the plain UDP flag; a TCP-only mux
/// falls back to native UDP when that flag is enabled.
#[cfg(feature = "mux")]
#[tokio::test]
async fn support_udp_follows_mux_capability() {
    const UUID: [u8; 16] = [
        0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08,
        0x11,
    ];
    let mk = |udp: bool| {
        VmessAdapter::new(
            "vmess-mux",
            "127.0.0.1",
            10086,
            UUID,
            Security::Aes128Gcm,
            udp,
            TransportChain::empty(),
            Arc::new(DirectDialer),
        )
    };
    let muxed = mk(false).with_mux(crate::mux::MuxOptions::default());
    assert!(muxed.support_udp(), "UDP-capable mux advertises");

    let only_tcp = mk(true).with_mux(crate::mux::MuxOptions {
        only_tcp: true,
        ..Default::default()
    });
    assert!(
        only_tcp.support_udp(),
        "only-tcp mux falls back to native UDP when enabled"
    );
}
