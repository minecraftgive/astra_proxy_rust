use super::*;

#[test]
fn force_start_is_explicit_for_local_and_remote_targets() {
    for local in [true, false] {
        assert!(matches!(
            call_plan(Action::ForceStart("proxy".into()), local).unwrap(),
            CallPlan::Execute {
                command: v::Command::ForceStart { instance }, ..
            } if instance == "proxy"
        ));
        assert!(matches!(
            call_plan(Action::Start("proxy".into()), local).unwrap(),
            CallPlan::Execute {
                command: v::Command::Start { instance }, ..
            } if instance == "proxy"
        ));
    }
}

#[test]
fn monitor_is_a_read_only_query_for_local_and_remote_targets() {
    for local in [true, false] {
        assert!(matches!(
            call_plan(Action::Monitor { name: "core".into() }, local).unwrap(),
            CallPlan::Request(v::RequestBody::Query(v::Query::Monitor { instance })) if instance == "core"
        ));
    }
}

#[test]
fn monitor_reply_maps_durable_v2_identity_to_the_tui_request_name() {
    let snapshot = v::MonitorSnapshot {
        instance: "cf85e3bf-f8e4-4090-b8c3-69193a0e1ff6".into(),
        pid: Some(42),
        memory_bytes: Some("9007199254740993".into()),
        ..Default::default()
    };
    let response = reply_for_action(v::ResponseBody::Monitor(snapshot), Some("proxy")).unwrap();
    let sample: v::MonitorSnapshot = serde_json::from_str(&response.text).unwrap();
    assert_eq!(sample.instance, "proxy");
    assert_eq!(sample.pid, Some(42));
    assert_eq!(sample.memory_bytes.as_deref(), Some("9007199254740993"));
}

#[test]
fn update_transaction_retains_the_callers_operation_identity() {
    let request = updates::UpdateRequest::new(updates::UpdateIntent::Rollback {
        target_directory: PathBuf::from("chosen-release"),
    });
    let expected = request.operation_id.clone();
    let CallPlan::Execute {
        operation_id,
        command: v::Command::Update { request },
    } = call_plan(Action::UpdateTransaction { request }, true).unwrap()
    else {
        panic!("An explicit update transaction must use the durable Execute path");
    };
    assert_eq!(operation_id, expected);
    assert_eq!(request.operation_id, expected);
    assert!(
        matches!(request.intent, updates::UpdateIntent::Rollback { target_directory }
        if target_directory == Path::new("chosen-release"))
    );
}

#[test]
fn save_and_rollback_defer_their_state_checks_until_execution() {
    let CallPlan::Save {
        name,
        original,
        text,
    } = call_plan(
        Action::Save {
            name: "proxy".into(),
            original: "expected source revision".into(),
            text: "edited document".into(),
        },
        true,
    )
    .unwrap()
    else {
        panic!("Saving must retain the original document for the draft conflict check");
    };
    assert_eq!(name, "proxy");
    assert_eq!(original, "expected source revision");
    assert_eq!(text, "edited document");
    assert!(matches!(
        call_plan(Action::Rollback, true).unwrap(),
        CallPlan::Rollback
    ));
}

#[test]
fn local_tun_enable_keeps_elevation_off_the_remote_execute_path() {
    let action = Action::Runtime {
        name: "proxy".into(),
        command: astra_model::runtime::Command::Tun(true),
        expected_pid: Some(123),
    };
    assert!(matches!(
        call_plan(action.clone(), true).unwrap(),
        CallPlan::Request(v::RequestBody::Local(v::LocalCommand::EnableTun {
            instance, expected_pid: Some(123)
        })) if instance == "proxy"
    ));
    assert!(matches!(
        call_plan(action, false).unwrap(),
        CallPlan::Execute { command: v::Command::Runtime {
            instance, command: v::RuntimeCommand::Tun(true), expected_pid: Some(123)
        }, .. } if instance == "proxy"
    ));
    assert!(matches!(
        call_plan(
            Action::Runtime {
                name: "proxy".into(),
                command: astra_model::runtime::Command::Tun(false),
                expected_pid: Some(123),
            },
            true
        )
        .unwrap(),
        CallPlan::Execute {
            command: v::Command::Runtime {
                command: v::RuntimeCommand::Tun(false),
                ..
            },
            ..
        }
    ));
}

#[test]
fn subscription_only_actions_cannot_be_sent_as_single_requests() {
    for action in [
        Action::Session,
        Action::FollowLogs {
            name: "proxy".into(),
            cursor: None,
        },
    ] {
        let Err(error) = call_plan(action, true) else {
            panic!("Subscription-only actions require the stream adapter");
        };
        assert!(error.to_string().contains("stream adapter"));
    }
}
