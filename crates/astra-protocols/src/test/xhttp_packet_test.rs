use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn layer() -> PacketUp {
    PacketUp(StreamUp {
        path: "/test".into(),
        host: "localhost".into(),
        scheme: "http",
        headers: vec![],
    })
}

async fn peer(
    server: tokio::io::DuplexStream,
    reject: bool,
    stall: bool,
    received: tokio::sync::mpsc::UnboundedSender<(u64, Vec<u8>)>,
) {
    let mut connection = h2::server::handshake(server).await.unwrap();
    let mut downloads = Vec::new();
    let mut workers = tokio::task::JoinSet::new();
    while let Some(Ok((request, mut response))) = connection.accept().await {
        if request.method() == "GET" {
            downloads.push(
                response
                    .send_response(http::Response::new(()), false)
                    .unwrap(),
            );
            continue;
        }
        assert_eq!(request.method(), "POST");
        let sequence: u64 = request
            .uri()
            .path()
            .rsplit('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let expected_length: usize = request.headers()["content-length"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let received = received.clone();
        workers.spawn(async move {
            let mut body = request.into_body();
            let mut payload = Vec::new();
            while let Some(data) = body.data().await {
                let data = data.unwrap();
                body.flow_control().release_capacity(data.len()).unwrap();
                payload.extend_from_slice(&data);
            }
            assert_eq!(expected_length, payload.len());
            received.send((sequence, payload)).unwrap();
            if stall {
                std::future::pending::<()>().await;
            }
            // Delay ACKs to exercise the bounded window and flush barrier.
            tokio::time::sleep(Duration::from_millis(10)).await;
            let status = if reject { 413 } else { 200 };
            response
                .send_response(
                    http::Response::builder().status(status).body(()).unwrap(),
                    true,
                )
                .unwrap();
        });
    }
    workers.shutdown().await;
}

#[tokio::test]
async fn packet_upload_flushes_all_sequences_and_refuses_fake_half_close() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, server) = tokio::io::duplex(8192);
        let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(peer(server, false, false, send));
        let mut stream = layer().connect(Box::new(client)).await.unwrap();
        let payload: Vec<u8> = (0..POST_BYTES * 6 + 37).map(|i| (i % 251) as u8).collect();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
        let mut packets = Vec::new();
        while let Ok(packet) = received.try_recv() {
            packets.push(packet);
        }
        packets.sort_by_key(|(sequence, _)| *sequence);
        assert_eq!(packets.len(), 7);
        let mut restored = Vec::new();
        for (index, (sequence, body)) in packets.into_iter().enumerate() {
            assert_eq!(sequence, index as u64);
            assert!(body.len() <= POST_BYTES);
            restored.extend(body);
        }
        assert_eq!(restored, payload);
        assert_eq!(
            stream.shutdown().await.unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        drop(stream);
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn rejected_post_fails_flush_and_read() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, server) = tokio::io::duplex(8192);
        let (send, _received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(peer(server, true, false, send));
        let mut stream = layer().connect(Box::new(client)).await.unwrap();
        stream.write_all(b"payload").await.unwrap();
        assert!(stream
            .flush()
            .await
            .unwrap_err()
            .to_string()
            .contains("413"));
        assert!(stream.read(&mut [0; 1]).await.is_err());
        drop(stream);
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelling_pending_post_releases_http_connection() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, server) = tokio::io::duplex(8192);
        let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(peer(server, false, true, send));
        let mut stream = layer().connect(Box::new(client)).await.unwrap();
        stream.write_all(b"payload").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stream.flush())
                .await
                .is_err()
        );
        assert_eq!(received.recv().await.unwrap().1, b"payload");
        drop(stream);
        server.await.unwrap();
    })
    .await
    .unwrap();
}
