use super::*;
#[test]
fn restart_acknowledges_only_a_durable_complete_session() {
    let root = std::env::temp_dir().join(format!(
        "astra-gui-checkpoint-{}-{}",
        std::process::id(),
        astra_ipc::progress::now_ms()
    ));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("session.json");
    let session = astra_client::state::Session {
        gui_windows: vec![6, 2, 5],
        input: "unsaved unicode 草稿".into(),
        ..Default::default()
    };
    assert!(matches!(
        checkpoint_for_restart(&path, 14, &session),
        BackendEvent::RestartCheckpoint {
            id: 14,
            result: Ok(())
        }
    ));
    let restored = astra_client::state::Session::load(&path).unwrap();
    assert_eq!(restored.gui_windows, vec![6, 2, 5]);
    assert_eq!(restored.input, session.input);
    assert!(matches!(
        checkpoint_for_restart(&root, 15, &session),
        BackendEvent::RestartCheckpoint {
            id: 15,
            result: Err(_)
        }
    ));
    assert_eq!(
        astra_client::state::Session::load(&path).unwrap().input,
        session.input
    );
    std::fs::remove_dir_all(root).unwrap();
}
