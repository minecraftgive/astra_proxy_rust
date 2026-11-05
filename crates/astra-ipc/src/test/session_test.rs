use super::*;
use astra_platform::platform::LocalListener;
async fn cancelled_call_closes_connection(during_handshake: bool) {
    let path = std::env::temp_dir().join(format!(
        "astra-cancel-{}-{during_handshake}.sock",
        std::process::id()
    ));
    let listener = LocalListener::bind(&path).unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (closed_tx, closed_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(
            crate::receive_management::<u8>(&mut stream).await.unwrap(),
            0
        );
        if !during_handshake {
            crate::send_management(&mut stream, &Event::<u8>::Ready(crate::API_VERSION))
                .await
                .unwrap();
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                1
            );
        }
        started_tx.send(()).unwrap();
        // No server-side timeout or reply can close this connection for the caller.
        assert!(stream.read_u8().await.is_err());
        closed_tx.send(()).unwrap();
        drop(stream);

        // The next call must use a fresh session, with no replay of the cancelled request.
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(
            crate::receive_management::<u8>(&mut stream).await.unwrap(),
            0
        );
        crate::send_management(&mut stream, &Event::<u8>::Ready(crate::API_VERSION))
            .await
            .unwrap();
        assert_eq!(
            crate::receive_management::<u8>(&mut stream).await.unwrap(),
            2
        );
        crate::send_management(&mut stream, &Event::Reply(2u8))
            .await
            .unwrap();
    });
    let peer = Peer::<u8, u8>::new(path.clone(), || 0);
    let first_peer = peer.clone();
    let first = tokio::spawn(async move { first_peer.request(1, Duration::from_secs(60)).await });
    tokio::time::timeout(Duration::from_secs(2), started_rx)
        .await
        .unwrap()
        .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), closed_rx)
        .await
        .expect("dropping the call must close its connection without waiting for a reply")
        .unwrap();
    assert_eq!(peer.request(2, Duration::from_secs(2)).await.unwrap(), 2);
    server.await.unwrap();
    astra_platform::platform::remove_socket(&path);
}

#[tokio::test]
async fn cancelling_during_handshake_closes_connection_and_reconnects() {
    cancelled_call_closes_connection(true).await;
}

#[tokio::test]
async fn cancelling_after_send_closes_connection_and_reconnects() {
    cancelled_call_closes_connection(false).await;
}

#[tokio::test]
async fn cancelling_queued_call_does_not_send_it_or_close_active_connection() {
    let path = std::env::temp_dir().join(format!("astra-queued-{}.sock", std::process::id()));
    let listener = LocalListener::bind(&path).unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(
            crate::receive_management::<u8>(&mut stream).await.unwrap(),
            0
        );
        crate::send_management(&mut stream, &Event::<u8>::Ready(crate::API_VERSION))
            .await
            .unwrap();
        assert_eq!(
            crate::receive_management::<u8>(&mut stream).await.unwrap(),
            1
        );
        started_tx.send(()).unwrap();
        release_rx.await.unwrap();
        crate::send_management(&mut stream, &Event::Reply(1u8))
            .await
            .unwrap();
        assert_eq!(
            crate::receive_management::<u8>(&mut stream).await.unwrap(),
            3
        );
        crate::send_management(&mut stream, &Event::Reply(3u8))
            .await
            .unwrap();
    });
    let peer = Peer::<u8, u8>::new(path.clone(), || 0);
    let first_peer = peer.clone();
    let first = tokio::spawn(async move { first_peer.request(1, Duration::from_secs(2)).await });
    tokio::time::timeout(Duration::from_secs(2), started_rx)
        .await
        .unwrap()
        .unwrap();
    let mut queued = Box::pin(peer.request(2, Duration::from_secs(2)));
    assert!(futures_util::poll!(queued.as_mut()).is_pending());
    drop(queued);
    release_tx.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), 1);
    assert_eq!(peer.request(3, Duration::from_secs(2)).await.unwrap(), 3);
    server.await.unwrap();
    astra_platform::platform::remove_socket(&path);
}

#[tokio::test]
async fn reuses_one_connection_and_does_not_replay_a_lost_mutation() {
    let path = std::env::temp_dir().join(format!("astra-peer-{}.sock", std::process::id()));
    let listener = LocalListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(
            crate::receive_management::<u8>(&mut stream).await.unwrap(),
            0
        );
        crate::send_management(&mut stream, &Event::<u8>::Ready(crate::API_VERSION))
            .await
            .unwrap();
        for value in [1u8, 2] {
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                value
            );
            crate::send_management(&mut stream, &Event::Reply(value))
                .await
                .unwrap();
        }
        assert_eq!(
            crate::receive_management::<u8>(&mut stream).await.unwrap(),
            3
        );
        drop(stream); // Mutation ran, but its reply was lost.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        );
    });
    let peer = Peer::<u8, u8>::new(path.clone(), || 0);
    assert_eq!(peer.request(1, Duration::from_secs(2)).await.unwrap(), 1);
    assert_eq!(peer.request(2, Duration::from_secs(2)).await.unwrap(), 2);
    assert!(peer.request(3, Duration::from_secs(2)).await.is_err());
    server.await.unwrap();
    astra_platform::platform::remove_socket(&path);
}
