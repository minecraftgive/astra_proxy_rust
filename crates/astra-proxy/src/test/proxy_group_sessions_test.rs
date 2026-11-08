use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test]
async fn poisoned_epochs_cancel_existing_leases_before_accepting_new_ones() {
    let sessions = Arc::new(Sessions::default());
    let old = sessions.lease(&["main".into()]);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = sessions.0.lock().unwrap();
        panic!("injected epoch update panic");
    }))
    .is_err());
    let next = sessions.lease(&["main".into()]);
    tokio::time::timeout(std::time::Duration::from_secs(1), old.cancelled())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), next.cancelled())
            .await
            .is_err()
    );
    assert!(!sessions.0.is_poisoned());
}
#[tokio::test]
async fn cancellation_is_scoped_wakes_idle_sockets_and_does_not_cancel_future_connections() {
    let sessions = Arc::new(Sessions::default());
    let (upstream, mut remote) = tokio::io::duplex(64);
    let mut client = sessions
        .lease(&["parent".into(), "child".into()])
        .wrap(Box::new(upstream));
    let unrelated = sessions.lease(&["other".into()]);
    client.write_all(b"hello").await.unwrap();
    let mut bytes = [0; 5];
    remote.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"hello");
    sessions.disconnect("child", "failure");
    let mut byte = [0];
    assert_eq!(client.read(&mut byte).await.unwrap(), 0);
    assert_eq!(
        client.write(b"x").await.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), remote.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(client.read(&mut byte).await.unwrap(), 0);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), unrelated.cancelled())
            .await
            .is_err()
    );
    let future = sessions.lease(&["child".into()]);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), future.cancelled())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn half_close_preserves_the_other_direction_and_drop_closes_upstream() {
    let sessions = Arc::new(Sessions::default());
    let (upstream, mut remote) = tokio::io::duplex(64);
    let mut client = sessions.lease(&["main".into()]).wrap(Box::new(upstream));
    client.write_all(b"request").await.unwrap();
    client.shutdown().await.unwrap();
    let mut request = Vec::new();
    remote.read_to_end(&mut request).await.unwrap();
    assert_eq!(request, b"request");
    remote.write_all(b"reply").await.unwrap();
    let mut reply = [0; 5];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"reply");
    drop(client);
    assert_eq!(
        remote.write(b"x").await.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}

#[tokio::test]
async fn cancellation_wakes_both_split_halves_under_backpressure() {
    let sessions = Arc::new(Sessions::default());
    let (upstream, _remote) = tokio::io::duplex(1);
    let client = sessions.lease(&["main".into()]).wrap(Box::new(upstream));
    let (mut reader, mut writer) = tokio::io::split(client);
    writer.write_all(b"x").await.unwrap();
    let read = tokio::spawn(async move { reader.read(&mut [0; 1]).await });
    let write = tokio::spawn(async move { writer.write(b"blocked").await });
    tokio::task::yield_now().await;
    sessions.disconnect("main", "test");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        assert_eq!(read.await.unwrap().unwrap(), 0);
        assert_eq!(
            write.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalidated_stream_can_be_polled_repeatedly() {
    let sessions = Arc::new(Sessions::default());
    let (upstream, _remote) = tokio::io::duplex(1);
    let mut client = sessions.lease(&["main".into()]).wrap(Box::new(upstream));
    sessions.invalidate();
    for _ in 0..2 {
        assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0);
        assert_eq!(
            client.flush().await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        client.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn lease_keeps_only_epochs_alive_after_configuration_owner_drops() {
    let sessions = Arc::new(Sessions::default());
    let weak = Arc::downgrade(&sessions);
    let (upstream, mut remote) = tokio::io::duplex(8);
    let mut client = sessions.lease(&["main".into()]).wrap(Box::new(upstream));
    drop(sessions);
    client.write_all(b"x").await.unwrap();
    assert_eq!(remote.read_u8().await.unwrap(), b'x');
    weak.upgrade().unwrap().disconnect("main", "test");
    assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0);
    drop(client);
    assert!(weak.upgrade().is_none());
}
