use crate::{
    config::{Listener, Transport},
    net::Stream,
};
use anyhow::{ensure, Result};
use bytes::{BufMut, BytesMut};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Tungstenite requires its concrete HTTP response as the handshake error.
#[allow(clippy::result_large_err)]
pub async fn accept(mut stream: Stream, l: &Listener) -> Result<Stream> {
    match l.transport {
        Transport::Tcp => Ok(stream),
        Transport::Ws => {
            let path = l.path.clone().unwrap_or_else(|| "/".into());
            let ws=tokio_tungstenite::accept_hdr_async(stream,move|request:&tokio_tungstenite::tungstenite::handshake::server::Request,response:tokio_tungstenite::tungstenite::handshake::server::Response|{
                if request.uri().path()!=path{return Err(http::Response::builder().status(404).body(Some("unknown path".into())).unwrap())}Ok(response)
            }).await?;
            let (client, bridge) = tokio::io::duplex(65536);
            let (mut rd, mut wr) = tokio::io::split(bridge);
            let (mut tx, mut rx) = ws.split();
            let read = tokio::spawn(async move {
                while let Some(Ok(message)) = rx.next().await {
                    match message {
                        tokio_tungstenite::tungstenite::Message::Binary(data) => {
                            if wr.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                        tokio_tungstenite::tungstenite::Message::Close(_) => break,
                        _ => {}
                    }
                }
                let _ = wr.shutdown().await;
            });
            let write = tokio::spawn(async move {
                let mut buf = BytesMut::new();
                loop {
                    buf.reserve(16384);
                    let Ok(n) = rd.read_buf(&mut (&mut buf).limit(16384)).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    if tx
                        .send(tokio_tungstenite::tungstenite::Message::Binary(
                            buf.split().freeze(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = tx.close().await;
            });
            Ok(Box::new(Bridge {
                stream: client,
                tasks: vec![read, write],
            }))
        }
        Transport::HttpUpgrade => {
            let mut header = vec![];
            while !header.ends_with(b"\r\n\r\n") {
                ensure!(header.len() < 8192, "HTTP upgrade header limit");
                header.push(stream.read_u8().await?);
            }
            let text = std::str::from_utf8(&header)?;
            let path = l.path.as_deref().unwrap_or("/");
            let mut lines = text.split("\r\n");
            ensure!(
                lines
                    .next()
                    .is_some_and(|v| v.starts_with(&format!("GET {path} HTTP/1.1"))),
                "HTTP upgrade path"
            );
            let mut upgrade = false;
            let mut connection = false;
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    if k.eq_ignore_ascii_case("upgrade") {
                        upgrade = v.trim().eq_ignore_ascii_case("websocket")
                    }
                    if k.eq_ignore_ascii_case("connection") {
                        connection = v
                            .split(',')
                            .any(|p| p.trim().eq_ignore_ascii_case("upgrade"))
                    }
                }
            }
            ensure!(upgrade && connection, "invalid HTTP upgrade");
            stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n").await?;
            Ok(stream)
        }
        Transport::Grpc | Transport::Xhttp => h2_tunnel(stream, l).await,
    }
}
async fn h2_tunnel(stream: Stream, l: &Listener) -> Result<Stream> {
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(1)
        .initial_window_size(1024 * 1024)
        .initial_connection_window_size(2 * 1024 * 1024)
        .max_header_list_size(8192);
    let mut conn = builder.handshake::<_, bytes::Bytes>(stream).await?;
    let (request, mut respond) = conn
        .accept()
        .await
        .ok_or_else(|| anyhow::anyhow!("missing gRPC request"))??;
    let grpc = l.transport == Transport::Grpc;
    let path = if grpc {
        format!("/{}/Tun", l.service_name.as_deref().unwrap_or("GunService"))
    } else {
        let p = l.path.as_deref().unwrap_or("/");
        format!("{}/", p.trim_end_matches('/'))
    };
    ensure!(
        request.method() == http::Method::POST && request.uri().path() == path,
        "gRPC method/path"
    );
    let mut send = respond.send_response(
        http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())?,
        false,
    )?;
    let mut recv = request.into_body();
    let (client, bridge) = tokio::io::duplex(65536);
    let (mut rd, mut wr) = tokio::io::split(bridge);
    let driver = tokio::spawn(async move {
        while let Some(request) = conn.accept().await {
            if let Ok((_, mut response)) = request {
                let _ = response.send_response(
                    http::Response::builder().status(429).body(()).unwrap(),
                    true,
                );
            }
        }
    });
    let read = tokio::spawn(async move {
        let mut buffer = vec![];
        while let Some(Ok(data)) = recv.data().await {
            if buffer.len() + data.len() > 1024 * 1024 {
                break;
            }
            let _ = recv.flow_control().release_capacity(data.len());
            if !grpc {
                if wr.write_all(&data).await.is_err() {
                    break;
                }
                continue;
            }
            buffer.extend(&data);
            loop {
                if buffer.len() < 5 {
                    break;
                }
                if buffer[0] != 0 {
                    return;
                }
                let n = u32::from_be_bytes(buffer[1..5].try_into().unwrap()) as usize;
                if n > 1024 * 1024 {
                    return;
                }
                if buffer.len() < 5 + n {
                    break;
                }
                let Ok(payload) = meow_transport::grpc::decode_gun_frame(&buffer[..5 + n]) else {
                    return;
                };
                if wr.write_all(payload).await.is_err() {
                    return;
                }
                buffer.drain(..5 + n);
            }
        }
        let _ = wr.shutdown().await;
    });
    let write = tokio::spawn(async move {
        let mut buf = BytesMut::new();
        loop {
            buf.reserve(16384);
            let Ok(n) = rd.read_buf(&mut (&mut buf).limit(16384)).await else {
                break;
            };
            if n == 0 {
                break;
            }
            let mut frame = if grpc {
                let frame = meow_transport::grpc::encode_gun_frame(&buf).into();
                buf.clear();
                frame
            } else {
                buf.split().freeze()
            };
            while !frame.is_empty() {
                send.reserve_capacity(frame.len());
                let Some(Ok(capacity)) =
                    futures_util::future::poll_fn(|cx| send.poll_capacity(cx)).await
                else {
                    return;
                };
                let n = capacity.min(frame.len());
                if n == 0 {
                    continue;
                }
                if send.send_data(frame.split_to(n), false).is_err() {
                    return;
                }
            }
        }
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
        let _ = send.send_trailers(trailers);
    });
    Ok(Box::new(Bridge {
        stream: client,
        tasks: vec![driver, read, write],
    }))
}
struct Bridge {
    stream: tokio::io::DuplexStream,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort()
        }
    }
}
impl tokio::io::AsyncRead for Bridge {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, b)
    }
}
impl tokio::io::AsyncWrite for Bridge {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, b)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub fn owned(stream: tokio::io::DuplexStream, tasks: Vec<tokio::task::AbortHandle>) -> Stream {
    Box::new(OwnedBridge { stream, tasks })
}
struct OwnedBridge {
    stream: tokio::io::DuplexStream,
    tasks: Vec<tokio::task::AbortHandle>,
}
impl Drop for OwnedBridge {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl tokio::io::AsyncRead for OwnedBridge {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, b)
    }
}
impl tokio::io::AsyncWrite for OwnedBridge {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, b)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
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
        }).await.unwrap();
    }

    #[tokio::test]
    async fn h2_preserves_data_and_grpc_framing_across_flow_control() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for transport in [Transport::Xhttp, Transport::Grpc] {
                let (client, server) = tokio::io::duplex(65536);
                let (mut client, connection) =
                    h2::client::handshake(client).await.unwrap();
                let driver = tokio::spawn(connection);
                let path = if transport == Transport::Grpc { "/GunService/Tun" } else { "/" };
                let (response, _request) = client.send_request(
                    http::Request::post(format!("http://localhost{path}")).body(()).unwrap(),
                    false,
                ).unwrap();
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
                            let frame = meow_transport::grpc::decode_gun_frame(&remaining[..5 + n]).unwrap();
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
        }).await.unwrap();
    }
}
