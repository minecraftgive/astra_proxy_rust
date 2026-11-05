use super::*;
#[tokio::test]
async fn dead_dns_listener_is_reported_with_transport_and_address() {
    let app = App::from_text(&std::env::temp_dir().join("astra-dns-task-test.toml"),
            "standalone=true\nstandalone_outbound='direct'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.direct]\nmembers=['DIRECT']\n".into()).unwrap();
    let current = Arc::new(arc_swap::ArcSwap::from_pointee(app));
    let prepared = Prepared::bind(&["127.0.0.1:0".parse().unwrap()], None)
        .await
        .unwrap();
    let mut guard = prepared.start(current);
    assert!(guard.failure().await.is_none());
    guard.tasks[0].abort();
    let failure = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(error) = guard.failure().await {
                break error;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(failure.contains("DNS UDP 127.0.0.1:"));
    assert!(failure.contains("cancelled"));
}
