use super::*;

fn listener(transport: Transport) -> Listener {
    let mut listener: Listener =
        toml::from_str("tag='test'\nprotocol='vless'\nlisten='127.0.0.1:0'").unwrap();
    listener.transport = transport;
    listener
}

#[tokio::test]
async fn websocket_preserves_data_across_bounded_frames() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (client, server) = tokio::io::duplex(65536);
        let listener = listener(Transport::Ws);
        let (server, client) = tokio::join!(
            accept(Box::new(server), &listener),
            tokio_tungstenite::client_async("ws://localhost/", client),
        );
        let mut server = server.unwrap();
        let (mut client, _) = client.unwrap();
        let payload: Vec<u8> = (0..200_003).map(|i| (i % 251) as u8).collect();
        let write = async {
            server.write_all(&payload).await.unwrap();
            server.shutdown().await.unwrap();
        };
        let read = async {
            let mut received = Vec::new();
            while let Some(message) = client.next().await {
                match message.unwrap() {
                    tokio_tungstenite::tungstenite::Message::Binary(data) => {
                        assert!(data.len() <= 16384);
                        received.extend_from_slice(&data);
                    }
                    tokio_tungstenite::tungstenite::Message::Close(_) => break,
                    other => panic!("unexpected message: {other:?}"),
                }
            }
            assert_eq!(received, payload);
        };
        tokio::join!(write, read);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn h2_preserves_data_and_grpc_framing_across_flow_control() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for transport in [Transport::Xhttp, Transport::Grpc] {
            let (client, server) = tokio::io::duplex(65536);
            let (mut client, connection) = h2::client::handshake(client).await.unwrap();
            let driver = tokio::spawn(connection);
            let path = if transport == Transport::Grpc {
                "/GunService/Tun"
            } else {
                "/"
            };
            let (response, _request) = client
                .send_request(
                    http::Request::post(format!("http://localhost{path}"))
                        .body(())
                        .unwrap(),
                    false,
                )
                .unwrap();
            let listener = listener(transport);
            let (server, response) = tokio::join!(accept(Box::new(server), &listener), response);
            let mut server = server.unwrap();
            let mut body = response.unwrap().into_body();
            let payload: Vec<u8> = (0..200_003).map(|i| (i % 251) as u8).collect();
            let write = async {
                server.write_all(&payload).await.unwrap();
                server.shutdown().await.unwrap();
            };
            let read = async {
                let mut received = Vec::new();
                while let Some(data) = body.data().await {
                    let data = data.unwrap();
                    body.flow_control().release_capacity(data.len()).unwrap();
                    received.extend_from_slice(&data);
                }
                assert_eq!(body.trailers().await.unwrap().unwrap()["grpc-status"], "0");
                if transport == Transport::Grpc {
                    let mut decoded = Vec::new();
                    let mut remaining = received.as_slice();
                    while !remaining.is_empty() {
                        let n = u32::from_be_bytes(remaining[1..5].try_into().unwrap()) as usize;
                        let frame =
                            meow_transport::grpc::decode_gun_frame(&remaining[..5 + n]).unwrap();
                        assert!(frame.len() <= 16384);
                        decoded.extend_from_slice(frame);
                        remaining = &remaining[5 + n..];
                    }
                    assert_eq!(decoded, payload);
                } else {
                    assert_eq!(received, payload);
                }
            };
            tokio::join!(write, read);
            driver.abort();
        }
    })
    .await
    .unwrap();
}
