use super::*;
use astra_client::operations::JobResult;
use gpui::{AppContext, TestAppContext};

#[gpui::test]
fn shared_models_can_spawn_and_wake_from_the_gpui_thread(cx: &mut TestAppContext) {
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    let (wake, notifications) = std::sync::mpsc::sync_channel(1);
    let waker = astra_client::waker::Waker::new(move || {
        let _ = wake.try_send(());
    });
    let missing_socket =
        std::env::temp_dir().join(format!("astra-gui-missing-manager-{}", std::process::id()));
    model.update(cx, |model, _| {
        model.tokio(|client| {
            client.viewer = Some(
                astra_client::logs::Viewer::new("test".into(), Default::default())
                    .with_waker(waker.clone()),
            );
            // Viewer::new is pure; Diagnostics also exercises the real spawn path.
            client.diagnostics = Some(astra_client::diagnostics::View::start_with_waker(
                &missing_socket,
                "test".into(),
                waker,
            ));
        });
    });
    notifications
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("spawned model must wake the frontend");
    model.update(cx, |model, _| {
        assert!(model.poll_views());
        assert!(model.client.viewer.is_some());
        assert!(model.client.diagnostics.as_ref().unwrap().error.is_some());
    });
}

#[gpui::test]
fn bridge_applies_current_results_and_drops_stale_runtime(cx: &mut TestAppContext) {
    let (commands, _) = async_channel::bounded(16);
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, cx| {
        let action = Action::Runtime {
            name: "test".into(),
            expected_pid: None,
            command: astra_model::runtime::Command::Snapshot,
        };
        let original = model.origin();
        let result = |count| JobResult::Runtime {
            name: "test".into(),
            snapshot: astra_model::runtime::Snapshot {
                active: count,
                ..Default::default()
            },
            message: String::new(),
        };
        model.apply(
            BackendEvent::JobDone {
                id: 1,
                action: Box::new(action.clone()),
                result: Box::new(result(12)),
                origin: original.clone(),
            },
            cx,
        );
        assert_eq!(model.client.runtime_snapshot.as_ref().unwrap().active, 12);
        model.client.page = 5;
        model.apply(
            BackendEvent::JobDone {
                id: 2,
                action: Box::new(action),
                result: Box::new(result(99)),
                origin: original,
            },
            cx,
        );
        assert_eq!(model.client.runtime_snapshot.as_ref().unwrap().active, 12);
        model.apply(
            BackendEvent::Disconnected {
                source: "Manager",
                message: "offline".into(),
            },
            cx,
        );
        assert!(!model.online);
        assert_eq!(model.connection_errors["Manager"], "offline");
        assert!(model.client.runtime_snapshot.is_some());
    });
}

#[test]
fn cross_executor_messages_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<BackendCmd>();
    assert_send::<BackendEvent>();
}

#[gpui::test]
fn async_bridge_delivers_disconnect_and_releases_closed_channel(cx: &mut TestAppContext) {
    let (commands, _) = async_channel::bounded(16);
    let (events, incoming) = async_channel::bounded(4);
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    cx.update(|cx| AppModel::bridge(&model, incoming, cx));
    events
        .try_send(BackendEvent::Disconnected {
            source: "Manager",
            message: "connection closed".into(),
        })
        .unwrap();
    events.close();
    cx.run_until_parked();
    model.update(cx, |model, _| {
        assert_eq!(model.connection_errors["Manager"], "connection closed")
    });
}

#[gpui::test]
fn quit_result_checkpoints_current_session_and_draft_before_closing(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    commands.try_send(BackendCmd::Cancel(7)).unwrap();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    cx.update(|cx| AppModel::observe_quit(&model, cx));
    let original = "standalone=true\n";
    let edited = "# unsaved edit\nstandalone=true\n";
    model.update(cx, |model, cx| {
        model.ready = true;
        model.client.active_profile = Some("proxy".into());
        model.client.input = "partially entered prompt".into();
        let mut editor = astra_client::editor::Editor::new("proxy".into(), original.into());
        editor.import(edited.into()).unwrap();
        model.client.editor = Some(editor);
        model.apply(
            BackendEvent::JobDone {
                id: 1,
                action: Box::new(Action::Shutdown),
                result: Box::new(JobResult::Done {
                    message: "Stopped".into(),
                    rows: None,
                    quit: true,
                }),
                origin: model.origin(),
            },
            cx,
        );
        assert!(!model.commands.is_closed());
    });
    assert!(matches!(incoming.try_recv(), Ok(BackendCmd::Cancel(7))));
    let BackendCmd::CheckpointForQuit { session } = incoming.try_recv().unwrap() else {
        panic!("quit result must enqueue a checkpoint");
    };
    assert_eq!(session.active_profile.as_deref(), Some("proxy"));
    assert_eq!(session.input, "partially entered prompt");
    let editor = session.editor.as_ref().unwrap();
    assert_eq!(editor.original, original);
    assert_eq!(editor.text(), edited);
    assert_eq!(session.drafts["proxy\n"].editor.text(), edited);
    // The native exit is allowed only after the backend confirms durable storage.
    assert!(incoming.is_empty());
    model.update(cx, |model, cx| {
        model.apply(BackendEvent::QuitCheckpoint(Ok(())), cx)
    });
    cx.update(|cx| cx.shutdown());
    assert!(matches!(
        incoming.try_recv(),
        Err(async_channel::TryRecvError::Closed)
    ));
}

#[gpui::test]
fn native_quit_checkpoints_the_loaded_session(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    cx.update(|cx| AppModel::observe_quit(&model, cx));
    model.update(cx, |model, _| {
        model.ready = true;
        model.client.active_profile = Some("native-quit-profile".into());
    });
    cx.update(|cx| cx.shutdown());
    let BackendCmd::Checkpoint(session) = incoming.try_recv().unwrap() else {
        panic!("native quit must enqueue a checkpoint");
    };
    assert_eq!(
        session.active_profile.as_deref(),
        Some("native-quit-profile")
    );
    assert!(matches!(
        incoming.try_recv(),
        Err(async_channel::TryRecvError::Closed)
    ));
}

#[gpui::test]
fn quitting_before_ready_does_not_overwrite_the_session_with_defaults(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, cx| model.request_quit(cx));
    assert!(matches!(
        incoming.try_recv(),
        Err(async_channel::TryRecvError::Closed)
    ));
}

#[gpui::test]
fn quit_reports_checkpoint_failure_when_backend_is_closed(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.update(gpui_component::init);
    let (commands, incoming) = async_channel::unbounded();
    incoming.close();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, cx| {
        model.ready = true;
        model.request_quit(cx);
        assert_eq!(
            model.client.message,
            astra_client::tr!("Cannot save session: {}", "sending into a closed channel")
        );
    });
}

fn restart_target() -> astra_client::windows::Context {
    astra_client::windows::Context {
        executable: "new/astra-gui".into(),
        home: "state".into(),
        config: "config".into(),
        bin: "new".into(),
        language: astra_client::i18n::Language::English,
    }
}
#[gpui::test]
fn failed_restart_checkpoint_preserves_live_interface_and_window_snapshot(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, _| {
        model.ready = true;
        model.register_window(8, 2);
        model.register_window(4, 6);
        model.client.input = "unsaved prompt".into();
        model.checkpoint_restart(restart_target());
    });
    let BackendCmd::CheckpointForRestart { id, session } = incoming.try_recv().unwrap() else {
        panic!("missing checkpoint")
    };
    assert_eq!(session.gui_windows, vec![6, 2]);
    assert_eq!(session.input, "unsaved prompt");
    model.update(cx, |model, cx| {
        model.apply(
            BackendEvent::RestartCheckpoint {
                id,
                result: Err("disk full".into()),
            },
            cx,
        );
        assert!(!model.commands.is_closed());
        assert!(!model.quit_prepared);
        assert!(model.restart_pending.is_none());
        assert!(model.restart_plan.lock().unwrap().is_none());
        assert!(model
            .client
            .recovery
            .as_ref()
            .unwrap()
            .message
            .contains("disk full"));
        assert_eq!(model.client.input, "unsaved prompt");
    });
}
#[gpui::test]
fn restart_waits_for_durable_latest_window_and_draft_snapshot(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, _| {
        model.ready = true;
        model.register_window(1, 0);
        model.register_window(2, 6);
        model.checkpoint_restart(restart_target());
        assert!(!model.commands.is_closed());
        model.update_window(2, 3);
        model.client.input = "new edit while saving".into();
    });
    let BackendCmd::CheckpointForRestart { id, .. } = incoming.try_recv().unwrap() else {
        panic!("missing checkpoint")
    };
    model.update(cx, |model, cx| {
        model.apply(BackendEvent::RestartCheckpoint { id, result: Ok(()) }, cx)
    });
    let BackendCmd::CheckpointForRestart {
        id: latest,
        session,
    } = incoming.try_recv().unwrap()
    else {
        panic!("changed state must be checkpointed again")
    };
    assert_ne!(latest, id);
    assert_eq!(session.gui_windows, vec![0, 3]);
    assert_eq!(session.input, "new edit while saving");
    model.update(cx, |model, cx| {
        model.apply(
            BackendEvent::RestartCheckpoint {
                id: latest,
                result: Ok(()),
            },
            cx,
        );
        assert!(model.commands.is_closed());
        assert!(model.quit_prepared);
        let plan = model.restart_plan.lock().unwrap();
        let (target, launch) = plan.as_ref().unwrap();
        assert_eq!(target.executable, std::path::PathBuf::from("new/astra-gui"));
        assert_eq!(launch.view, Some(astra_client::windows::View::Dashboard));
    });
}
#[gpui::test]
fn cancelled_tasks_finish_and_late_results_do_not_open_views(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, cx| {
        model.start(Action::UpdateStatus, cx);
        let id = model.next_job;
        model.cancel_task(id, cx);
        assert!(model
            .client
            .tasks
            .records
            .back()
            .unwrap()
            .finished
            .is_some());
        model.apply(
            BackendEvent::JobDone {
                id,
                action: Box::new(Action::UpdateStatus),
                result: Box::new(JobResult::Done {
                    message: "late result".into(),
                    rows: None,
                    quit: false,
                }),
                origin: model.origin(),
            },
            cx,
        );
        assert_ne!(model.client.message, "late result");
    });
    assert!(matches!(incoming.try_recv(), Ok(BackendCmd::Start(_))));
    assert!(matches!(incoming.try_recv(), Ok(BackendCmd::Cancel(1))));
}

#[gpui::test]
fn task_history_caps_final_events_and_rejects_a_full_running_queue(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, cx| {
        for id in 1..=32 {
            model
                .client
                .tasks
                .records
                .push_back(astra_client::tasks::Record {
                    id,
                    title: "test".into(),
                    started: std::time::Instant::now(),
                    finished: None,
                    events: std::iter::repeat_n("progress".into(), 128).collect(),
                });
        }
        model.next_job = 32;
        model.start(Action::UpdateStatus, cx);
        assert_eq!(model.client.tasks.records.len(), 32);
        assert!(incoming.try_recv().is_err());
        assert!(model.client.tasks.verdict.as_ref().unwrap().1);
        model.apply(
            BackendEvent::Rejected {
                id: 1,
                message: "rejected for test".into(),
            },
            cx,
        );
        let first = model.client.tasks.records.front().unwrap();
        assert_eq!(first.events.len(), 128);
        assert_eq!(first.events.back().unwrap(), "rejected for test");
        assert_eq!(
            model.client.tasks.verdict.as_ref(),
            Some(&("rejected for test".into(), true))
        );
        model.start(Action::UpdateStatus, cx);
        assert_eq!(model.client.tasks.records.len(), 32);
        assert!(model.client.tasks.records.iter().all(|task| task.id != 1));
        assert!(matches!(incoming.try_recv(), Ok(BackendCmd::Start(_))));
        let origin = model.origin();
        model.apply(
            BackendEvent::JobDone {
                id: 2,
                action: Box::new(Action::UpdateStatus),
                result: Box::new(JobResult::Done {
                    message: "completed for test".into(),
                    rows: None,
                    quit: false,
                }),
                origin,
            },
            cx,
        );
        let task = model
            .client
            .tasks
            .records
            .iter()
            .find(|task| task.id == 2)
            .unwrap();
        assert_eq!(task.events.len(), 128);
        assert_eq!(task.events.back().unwrap(), "completed for test");
    });
}
