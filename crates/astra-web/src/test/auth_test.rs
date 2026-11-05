use super::*;
#[tokio::test]
async fn credentials_sessions_revocation_and_password_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("admin.phc");
    assert!(Auth::new(file.clone()).is_err());
    set_password(&file, "a long test password").unwrap();
    let auth = Auth::new(file.clone()).unwrap();
    assert!(auth.login("wrong".into()).await.is_err());
    let token = auth.login("a long test password".into()).await.unwrap();
    assert!(auth.verify(&token));
    assert!(!auth.verify(&"0".repeat(64)));
    auth.logout(&token);
    assert!(!auth.verify(&token));
    let expired = auth.login("a long test password".into()).await.unwrap();
    auth.sessions
        .lock()
        .unwrap()
        .get_mut(&digest(expired.as_bytes()))
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    assert!(!auth.verify(&expired));
    let token = auth.login("a long test password".into()).await.unwrap();
    set_password(&file, "a different test password").unwrap();
    assert!(!auth.verify(&token));
    let restarted = Auth::new(file).unwrap();
    assert!(!restarted.verify(&token));
    for _ in 0..5 {
        assert!(auth.permit_attempt("127.0.0.1".parse().unwrap()));
    }
    assert!(!auth.permit_attempt("127.0.0.1".parse().unwrap()));
}
