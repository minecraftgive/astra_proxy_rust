use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn pair(
    security: Security,
    capacity: usize,
) -> (
    VmessPacketConn,
    tokio::io::DuplexStream,
    header::SealedHeader,
) {
    let sealed = header::seal_request_header(
        &[1; 16],
        security,
        &meow_common::Metadata {
            host: "example.com".into(),
            dst_port: 53,
            ..Default::default()
        },
        true,
    )
    .unwrap();
    let (client, peer) = tokio::io::duplex(capacity);
    (
        VmessPacketConn::new(Box::new(client), security, &sealed),
        peer,
        sealed,
    )
}

#[tokio::test]
async fn client_first_packets_keep_boundaries_and_drain_short_buffers() {
    for security in [Security::Aes128Gcm, Security::ChaCha20Poly1305] {
        let (conn, mut peer, sealed) = pair(security, 64);
        let server = tokio::spawn(async move {
            let mut cipher = BodyCipher::server_mirror(security, &sealed.req_key, &sealed.req_iv);
            // Wait for the client first, as actual UDP echo services do.
            let first = cipher.read_record(&mut peer).await.unwrap().unwrap();
            assert_eq!(first, b"first");
            let response =
                header::tests::seal_response_header(&sealed.req_key, &sealed.req_iv, sealed.resp_v);
            for chunk in response.chunks(3) {
                peer.write_all(chunk).await.unwrap();
            }
            for data in [vec![42; 8192], vec![], b"last".to_vec()] {
                cipher.write_record(&mut peer, &data).await.unwrap();
            }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            conn.write_packet(b"first", &"127.0.0.1:53".parse().unwrap())
                .await
                .unwrap();
            let mut short = [0; 4];
            assert!(conn
                .read_packet(&mut short)
                .await
                .unwrap_err()
                .to_string()
                .contains("receive buffer"));
            assert_eq!(conn.read_packet(&mut short).await.unwrap().0, 0);
            assert_eq!(conn.read_packet(&mut short).await.unwrap().0, 4);
            assert_eq!(&short, b"last");
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn canceled_partial_header_and_write_retire_session() {
    let (conn, mut peer, _) = pair(Security::Aes128Gcm, 8);
    peer.write_all(&[1]).await.unwrap();
    let mut packet = [0; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(20), conn.read_packet(&mut packet))
            .await
            .is_err()
    );
    let addr = "127.0.0.1:53".parse().unwrap();
    assert!(conn.write_packet(b"next", &addr).await.is_err());
    assert!(conn.read_packet(&mut packet).await.is_err());
    drop(conn);
    assert_eq!(peer.read(&mut packet).await.unwrap(), 0);

    let (conn, mut peer, _) = pair(Security::ChaCha20Poly1305, 8);
    // A partially written frame cannot be resumed by a new packet.
    assert!(tokio::time::timeout(
        Duration::from_millis(20),
        conn.write_packet(&[42; 100], &addr)
    )
    .await
    .is_err());
    assert!(conn.write_packet(b"next", &addr).await.is_err());
    drop(conn);
    let mut written = Vec::new();
    peer.read_to_end(&mut written).await.unwrap();
    assert_eq!(written.len(), 8);
}
