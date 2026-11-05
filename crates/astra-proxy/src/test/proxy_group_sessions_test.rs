use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test]
async fn poisoned_epochs_cancel_existing_leases_before_accepting_new_ones() {
    let sessions = Sessions::default();
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
    let sessions = Sessions::default();
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
