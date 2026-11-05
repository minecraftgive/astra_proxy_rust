use super::*;
#[tokio::test]
async fn shrinking_keeps_old_sessions_and_blocks_new_admissions() {
    let quota = Arc::new(Limit::new(2));
    let first = quota.clone().try_acquire_owned().unwrap();
    let second = quota.clone().try_acquire_owned().unwrap();
    quota.resize(1);
    assert!(quota.clone().try_acquire_owned().is_err());
    drop(first);
    assert!(quota.clone().try_acquire_owned().is_err());
    drop(second);
    let permit = quota.clone().try_acquire_owned().unwrap();
    quota.resize(2);
    assert!(quota.clone().try_acquire_owned().is_ok());
    drop(permit);
    quota.idle().await;
}
