use super::*;
use crate::vmess::header::tests::seal_response_header;
use crate::vmess::header::Security;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn forwards_request_body_before_response_header_arrives() {
    let req_key = [0x11; 16];
    let req_iv = [0x22; 16];
    let resp_v = 0x5a;
    let (transport, mut server) = tokio::io::duplex(4096);
    let mut app = spawn_vmess_relay(
        Box::new(transport),
        Security::Aes128Gcm,
        req_key,
        req_iv,
        resp_v,
    );

    app.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();

    // A real server does not send the response header until it has read
    // the request. The relay must therefore forward a body record first.
    let mut len = [0u8; 2];
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        server.read_exact(&mut len),
    )
    .await
    .expect("request body was blocked behind response-header read")
    .unwrap();
    let mut ciphertext = vec![0u8; u16::from_be_bytes(len) as usize];
    server.read_exact(&mut ciphertext).await.unwrap();
    assert_eq!(ciphertext.len(), b"GET / HTTP/1.1\r\n\r\n".len() + 16);

    server
        .write_all(&seal_response_header(&req_key, &req_iv, resp_v))
        .await
        .unwrap();
}

/// Issue #514: dropping the returned endpoint must abort both relay
/// tasks and release the transport even when the upstream stays silent
/// (no data, FIN, or RST). Before the fix the read task parked on
/// `read_record` forever and pinned the transport open.
#[tokio::test]
async fn drop_releases_silent_upstream_transport() {
    let req_key = [0x11; 16];
    let req_iv = [0x22; 16];
    let (transport, mut server) = tokio::io::duplex(4096);
    let app = spawn_vmess_relay(
        Box::new(transport),
        Security::Aes128Gcm,
        req_key,
        req_iv,
        0x5a,
    );
    drop(app);

    tokio::time::timeout(std::time::Duration::from_secs(2), async move {
        // The write half of the transport is held by the write task: once
        // aborted, reads on the server end see EOF.
        assert_eq!(
            server.read(&mut [0u8; 1]).await.unwrap(),
            0,
            "transport write half must be released"
        );
        // The read half is held by the read task parked on the silent
        // upstream: once aborted, writes on the server end must fail
        // instead of buffering into a dead peer.
        loop {
            match server.write(b"x").await {
                Err(_) => break,
                Ok(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("relay tasks must be aborted promptly on drop");
}

/// Issue #514 counterpart: a write-side half-close must NOT abort the
/// read direction — a legitimate half-close download still completes.
#[tokio::test]
async fn write_half_close_keeps_read_side_open() {
    let req_key = [0x11; 16];
    let req_iv = [0x22; 16];
    let resp_v = 0x5a;
    let (transport, mut server) = tokio::io::duplex(4096);
    let mut app = spawn_vmess_relay(Box::new(transport), Security::None, req_key, req_iv, resp_v);

    // Client half-closes its write direction (e.g. request fully sent).
    app.shutdown().await.unwrap();
    // The transport's write half is shut down: the server observes EOF.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        assert_eq!(server.read(&mut [0u8; 1]).await.unwrap(), 0);
    })
    .await
    .expect("server must observe the client half-close");

    // The server can still answer: header + one plain (Security::None)
    // record must reach the client through the still-open read half.
    server
        .write_all(&seal_response_header(&req_key, &req_iv, resp_v))
        .await
        .unwrap();
    let payload = b"late download bytes";
    server
        .write_all(&(payload.len() as u16).to_be_bytes())
        .await
        .unwrap();
    server.write_all(payload).await.unwrap();

    let mut out = vec![0u8; payload.len()];
    tokio::time::timeout(std::time::Duration::from_secs(2), app.read_exact(&mut out))
        .await
        .expect("read side must stay open after write half-close")
        .unwrap();
    assert_eq!(&out, payload);
}

/// Issue #514 review follow-up: the reverse half-close — when the
/// server sends FIN at a record boundary (clean EOF on the read side),
/// the write direction must stay alive: a client still uploading must
/// keep reaching the server instead of having its writer task aborted
/// with the read loop.
#[tokio::test]
async fn upstream_half_close_keeps_write_side_open() {
    let req_key = [0x11; 16];
    let req_iv = [0x22; 16];
    let resp_v = 0x5a;
    let (transport, mut server) = tokio::io::duplex(4096);
    let mut app = spawn_vmess_relay(Box::new(transport), Security::None, req_key, req_iv, resp_v);

    // Server answers the response header, then half-closes its send
    // direction while continuing to read uploads.
    server
        .write_all(&seal_response_header(&req_key, &req_iv, resp_v))
        .await
        .unwrap();
    server.shutdown().await.unwrap();

    // The client observes EOF on its read direction.
    let mut sink = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        app.read_to_end(&mut sink),
    )
    .await
    .expect("client read side must observe the server FIN")
    .unwrap();

    // Give the supervisor a scheduling slice: under the buggy version
    // it aborts the write task as soon as the read loop ends.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // A post-FIN upload must still reach the server.
    app.write_all(b"post-fin upload").await.unwrap();
    let mut len = [0u8; 2];
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        server.read_exact(&mut len),
    )
    .await
    .expect("write side must stay open after upstream half-close")
    .unwrap();
    let mut rec = vec![0u8; u16::from_be_bytes(len) as usize];
    server.read_exact(&mut rec).await.unwrap();
    assert_eq!(&rec, b"post-fin upload");
}

/// Issue #514 review: a FIN landing MID-record is not a half-close —
/// it is a corrupt session. The read task must classify it fatal so
/// the supervisor aborts the write side instead of letting uploads
/// keep pumping into a broken connection. Covers both truncation
/// points: inside the length prefix and inside the body.
#[tokio::test]
async fn upstream_truncated_record_aborts_write_side() {
    for tail in [
        // One byte of the two-byte length prefix, then FIN.
        vec![0x00u8].as_slice(),
        // Full prefix promising 8 body bytes, only 3 arrive, then FIN.
        [0x00, 0x08, 1, 2, 3].as_slice(),
    ] {
        let req_key = [0x11; 16];
        let req_iv = [0x22; 16];
        let resp_v = 0x5a;
        let (transport, mut server) = tokio::io::duplex(4096);
        let mut app =
            spawn_vmess_relay(Box::new(transport), Security::None, req_key, req_iv, resp_v);

        server
            .write_all(&seal_response_header(&req_key, &req_iv, resp_v))
            .await
            .unwrap();
        server.write_all(tail).await.unwrap();
        server.shutdown().await.unwrap();

        // The client still observes EOF on its read direction.
        let mut sink = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            app.read_to_end(&mut sink),
        )
        .await
        .expect("client read side must observe the upstream close")
        .unwrap();

        // Fatal classification ⇒ supervisor aborted the write task:
        // the transport's write half is released, so the server's read
        // side sees EOF — and client writes fail instead of
        // disappearing into a dead session.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            assert_eq!(
                server.read(&mut [0u8; 1]).await.unwrap(),
                0,
                "transport write half must be released after truncation"
            );
        })
        .await
        .expect("write task must be aborted after truncation");
        app.write_all(b"late upload")
            .await
            .expect_err("writes must fail once the session is dead");
    }
}
