use super::*;
use std::time::Duration;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

const LINGER: Duration = Duration::from_millis(200);

#[tokio::test]
async fn completes_when_both_directions_close() {
    let (mut client, mut client_side) = duplex(1024);
    let (mut server_side, mut server) = duplex(1024);
    let relay =
        tokio::spawn(async move { relay_with(&mut client_side, &mut server_side, LINGER).await });
    client.write_all(b"ping").await.unwrap();
    client.shutdown().await.unwrap();
    let mut request = Vec::new();
    server.read_to_end(&mut request).await.unwrap();
    assert_eq!(request, b"ping");
    server.write_all(b"pong!").await.unwrap();
    server.shutdown().await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert_eq!(response, b"pong!");
    assert_eq!(relay.await.unwrap().unwrap(), (4, 5));
}

#[tokio::test]
async fn closes_silent_upstream_after_client_half_close() {
    let (mut client, mut client_side) = duplex(1024);
    let (mut server_side, server) = duplex(1024);
    let relay =
        tokio::spawn(async move { relay_with(&mut client_side, &mut server_side, LINGER).await });
    client.write_all(b"bye").await.unwrap();
    client.shutdown().await.unwrap();
    // The upstream stays open and never answers, as a proxied server may.
    let totals = tokio::time::timeout(LINGER * 10, relay)
        .await
        .expect("half-closed relay outlived its linger")
        .unwrap()
        .unwrap();
    assert_eq!(totals, (3, 0));
    drop(server);
}

#[tokio::test]
async fn closes_silent_client_after_upstream_half_close() {
    let (client, mut client_side) = duplex(1024);
    let (mut server_side, mut server) = duplex(1024);
    let relay =
        tokio::spawn(async move { relay_with(&mut client_side, &mut server_side, LINGER).await });
    server.write_all(b"done").await.unwrap();
    server.shutdown().await.unwrap();
    let totals = tokio::time::timeout(LINGER * 10, relay)
        .await
        .expect("half-closed relay outlived its linger")
        .unwrap()
        .unwrap();
    assert_eq!(totals, (0, 4));
    drop(client);
}

#[tokio::test]
async fn active_surviving_direction_outlives_linger() {
    let (mut client, mut client_side) = duplex(1024);
    let (mut server_side, mut server) = duplex(1024);
    let relay =
        tokio::spawn(async move { relay_with(&mut client_side, &mut server_side, LINGER).await });
    client.shutdown().await.unwrap();
    // Stream a response for three lingers, never pausing a full linger.
    for _ in 0..12 {
        server.write_all(b"x").await.unwrap();
        tokio::time::sleep(LINGER / 4).await;
    }
    server.shutdown().await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert_eq!(response.len(), 12);
    assert_eq!(relay.await.unwrap().unwrap(), (0, 12));
}

#[tokio::test]
async fn idle_open_relay_is_not_closed() {
    let (mut client, mut client_side) = duplex(1024);
    let (mut server_side, mut server) = duplex(1024);
    let relay =
        tokio::spawn(async move { relay_with(&mut client_side, &mut server_side, LINGER).await });
    // Long-lived idle connections (SSH, IMAP IDLE) must survive silence.
    tokio::time::sleep(LINGER * 4).await;
    assert!(!relay.is_finished());
    client.write_all(b"still here").await.unwrap();
    let mut echoed = [0; 10];
    server.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"still here");
    relay.abort();
}

#[tokio::test]
async fn closes_half_closed_tcp_relay_whose_client_reset() {
    use tokio::net::{TcpListener, TcpStream};
    let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let back = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (front_addr, back_addr) = (front.local_addr().unwrap(), back.local_addr().unwrap());
    let relay = tokio::spawn(async move {
        let (mut inbound, _) = front.accept().await.unwrap();
        let mut upstream = TcpStream::connect(back_addr).await.unwrap();
        relay_with(&mut inbound, &mut upstream, LINGER).await
    });
    let mut client = TcpStream::connect(front_addr).await.unwrap();
    let (_silent_server, _) = back.accept().await.unwrap();
    client.write_all(b"GET").await.unwrap();
    client.shutdown().await.unwrap();
    // Let the relay see the FIN (CLOSE_WAIT) before the reset (CLOSED).
    tokio::time::sleep(LINGER / 4).await;
    socket2::SockRef::from(&client)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(client);
    // Ending by linger (Ok) or by noticing the reset (Err) both free the socket.
    let _ = tokio::time::timeout(LINGER * 10, relay)
        .await
        .expect("relay kept a reset client's descriptor")
        .unwrap();
}
