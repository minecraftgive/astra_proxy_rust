use super::*;
use crate::protocol::updates::{UpdateIntent, UpdateRequest, UpdateStatus};

#[tokio::test]
async fn a_subscriber_close_before_the_next_push_is_a_normal_disconnect() {
    use tokio::io::AsyncReadExt;
    let (client, mut server) = astra_platform::platform::stream_pair().await.unwrap();
    drop(client);
    // Observe the close first, then exercise a write that raced with it.
    assert!(timeout(Duration::from_millis(500), server.read_u8())
        .await
        .unwrap()
        .is_err());
    assert!(
        !send_subscription(&mut server, &Reply::text("next snapshot"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn read_and_watch_update_status_do_not_enter_the_mutation_queue() {
    let id = UpdateRequest::new(UpdateIntent::Stage {
        package: PathBuf::new(),
    })
    .operation_id;
    let root = std::env::temp_dir().join(format!("astra-update-watch-{id}"));
    let manager = Manager::load(root.join("state"), root.join("bundle")).unwrap();
    let hub = Hub::new(&manager).unwrap();
    // Keep the receiver alive but deliberately never service it, as during a long update.
    let (sender, mut queued) = mpsc::channel(1);
    let (stopped, _stop) = mpsc::channel(1);
    for action in [Action::UpdateStatus, Action::WatchUpdates] {
        let watching = matches!(action, Action::WatchUpdates);
        let (mut client, server) = astra_platform::platform::stream_pair().await.unwrap();
        let task = tokio::spawn(serve(
            server,
            hub.clone(),
            sender.clone(),
            stopped.clone(),
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
        ));
        astra_ipc::send_management(
            &mut client,
            &Request {
                version: astra_ipc::API_VERSION,
                action,
            },
        )
        .await
        .unwrap();
        let first: Reply = timeout(
            Duration::from_millis(500),
            astra_ipc::receive_management(&mut client),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::from_str::<UpdateStatus>(&first.text)
                .unwrap()
                .schema_version,
            1
        );
        if watching {
            let mut changed = manager.cached_update_status();
            changed.staged_release_id = Some("observed-during-work".into());
            hub.publish_updates(changed);
            timeout(Duration::from_millis(500), async {
                loop {
                    let next: Reply = astra_ipc::receive_management(&mut client).await.unwrap();
                    let status: UpdateStatus = serde_json::from_str(&next.text).unwrap();
                    if status.staged_release_id.as_deref() == Some("observed-during-work") {
                        break;
                    }
                }
            })
            .await
            .unwrap();
        }
        drop(client);
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert!(queued.try_recv().is_err());
    assert!(
        !manager.home.join("updates").exists(),
        "reads and watches must not open/recover release-store journals"
    );
    drop(hub);
    drop(manager);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn gate_requires_observed_identity_and_idle_host() {
    let mut status = UpdateStatus::default();
    status.capabilities.experimental_enabled = true;
    status.capabilities.ordinary_user = true;
    status.instances.insert(
        "legacy".into(),
        crate::protocol::updates::UpdateInstance {
            reachable: true,
            status: Some(serde_json::json!({"active":1})),
            error: None,
            ..Default::default()
        },
    );
    apply_instance_gate(&mut status);
    assert!(!status.capabilities.activation_allowed);
    status.instances.get_mut("legacy").unwrap().status =
        Some(serde_json::json!({"boot_id":"one","paused":true}));
    apply_instance_gate(&mut status);
    assert!(!status.capabilities.activation_allowed);
    status.instances.get_mut("legacy").unwrap().status =
        Some(serde_json::json!({"boot_id":"one","paused":false,"busy":false}));
    apply_instance_gate(&mut status);
    assert!(status.capabilities.activation_allowed);
}
