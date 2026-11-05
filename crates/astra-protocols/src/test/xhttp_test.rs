use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn rejected_upload_fails_download_and_releases_connection() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client, server) = tokio::io::duplex(8192);
        let peer = tokio::spawn(async move {
            let mut connection = h2::server::handshake(server).await.unwrap();
            let mut downloads = Vec::new();
            while let Some(Ok((request, mut response))) = connection.accept().await {
                if request.method() == "GET" {
                    downloads.push(
                        response
                            .send_response(http::Response::new(()), false)
                            .unwrap(),
                    );
                } else {
                    response
                        .send_response(
                            http::Response::builder().status(403).body(()).unwrap(),
                            true,
                        )
                        .unwrap();
                }
            }
        });
        let layer = StreamUp {
            path: "/test".into(),
            host: "localhost".into(),
            scheme: "http",
            headers: vec![],
        };
        let mut stream = layer.connect(Box::new(client)).await.unwrap();
        let mut response = Vec::new();
        assert!(stream.read_to_end(&mut response).await.is_err());
        drop(stream);
        peer.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancel_pending_download_then_drop_releases_http_tasks() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (client, server) = tokio::io::duplex(8192);
        let (ready, observed) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let mut connection = h2::server::handshake(server).await.unwrap();
            let mut requests = Vec::new();
            let mut ready = Some(ready);
            while let Some(Ok(request)) = connection.accept().await {
                requests.push(request);
                if requests.len() == 2 {
                    ready.take().unwrap().send(()).unwrap();
                }
            }
        });
        let layer = StreamUp {
            path: "/test".into(),
            host: "localhost".into(),
            scheme: "http",
            headers: vec![],
        };
        let mut stream = layer.connect(Box::new(client)).await.unwrap();
        observed.await.unwrap();
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stream.read(&mut byte))
                .await
                .is_err()
        );
        drop(stream);
        peer.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn response_survives_upload_half_close() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client, server) = tokio::io::duplex(8192);
        let peer = tokio::spawn(async move {
            let mut connection = h2::server::handshake(server).await.unwrap();
            let (get, mut response) = connection.accept().await.unwrap().unwrap();
            assert_eq!(get.method(), "GET");
            let session_path = get.uri().path().to_owned();
            let mut down = response
                .send_response(http::Response::new(()), false)
                .unwrap();
            let (post, mut response) = connection.accept().await.unwrap().unwrap();
            assert_eq!(post.method(), "POST");
            assert_eq!(post.uri().path(), session_path);
            let worker = tokio::spawn(async move {
                let mut up_response = response
                    .send_response(http::Response::new(()), false)
                    .unwrap();
                let mut body = post.into_body();
                let mut received = Vec::new();
                while let Some(data) = body.data().await {
                    let data = data.unwrap();
                    body.flow_control().release_capacity(data.len()).unwrap();
                    received.extend_from_slice(&data);
                }
                assert_eq!(received, b"request");
                // The response is deliberately sent only AFTER upload EOF.
                down.send_data(bytes::Bytes::from_static(b"response after EOF"), true)
                    .unwrap();
                up_response.send_data(bytes::Bytes::new(), true).unwrap();
            });
            while connection.accept().await.is_some() {}
            worker.await.unwrap();
        });
        let layer = StreamUp {
            path: "/test".into(),
            host: "localhost".into(),
            scheme: "http",
            headers: vec![],
        };
        let mut stream = layer.connect(Box::new(client)).await.unwrap();
        stream.write_all(b"request").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response after EOF");
        drop(stream);
        peer.await.unwrap();
    })
    .await
    .unwrap();
}
