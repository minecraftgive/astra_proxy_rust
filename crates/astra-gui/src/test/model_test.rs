use super::*;
use astra_client::operations::JobResult;
use gpui::{AppContext, TestAppContext};

#[gpui::test]
fn bridge_applies_current_results_and_drops_stale_runtime(cx: &mut TestAppContext) {
    let (commands, _) = async_channel::bounded(16);
    let model = cx.new(|_| AppModel::new(Kind::Proxy, commands));
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
    let model = cx.new(|_| AppModel::new(Kind::Proxy, commands));
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
    let model = cx.new(|_| AppModel::new(Kind::Proxy, commands));
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
        assert!(model.commands.is_closed());
    });
    // The native quit observer runs after an explicit quit request and must not
    // enqueue a second checkpoint or try to write into the now-closed channel.
    cx.update(|cx| cx.shutdown());
    assert!(matches!(incoming.try_recv(), Ok(BackendCmd::Cancel(7))));
    let BackendCmd::Checkpoint(session) = incoming.try_recv().unwrap() else {
        panic!("quit result must enqueue a checkpoint");
    };
    assert_eq!(session.active_profile.as_deref(), Some("proxy"));
    assert_eq!(session.input, "partially entered prompt");
    let editor = session.editor.as_ref().unwrap();
    assert_eq!(editor.original, original);
    assert_eq!(editor.text(), edited);
    assert_eq!(session.drafts["proxy\n"].editor.text(), edited);
    assert!(matches!(
        incoming.try_recv(),
        Err(async_channel::TryRecvError::Closed)
    ));
}

#[gpui::test]
fn native_quit_checkpoints_the_loaded_session(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new(Kind::Proxy, commands));
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
    let model = cx.new(|_| AppModel::new(Kind::Proxy, commands));
    model.update(cx, |model, cx| model.request_quit(cx));
    assert!(matches!(
        incoming.try_recv(),
        Err(async_channel::TryRecvError::Closed)
    ));
}

#[gpui::test]
fn quit_reports_checkpoint_failure_when_backend_is_closed(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    incoming.close();
    let model = cx.new(|_| AppModel::new(Kind::Proxy, commands));
    model.update(cx, |model, cx| {
        model.ready = true;
        model.request_quit(cx);
        assert_eq!(
            model.client.message,
            astra_client::tr!("Cannot save session: {}", "sending into a closed channel")
        );
    });
}
