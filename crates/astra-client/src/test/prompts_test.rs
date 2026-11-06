use super::*;
#[test]
fn invalid_prompt_stays_open_with_exact_input_for_inline_correction() {
    let mut client = Client::new(Kind::Rule);
    client.prompt(Prompt::AddName);
    let error = client
        .submit_prompt("name with spaces".into())
        .err()
        .expect("invalid name");
    assert!(matches!(error.prompt, Prompt::AddName));
    assert_eq!(error.input, "name with spaces");
    assert!(matches!(client.prompt, Some(Prompt::AddName)));
    assert_eq!(client.input, "name with spaces");
    let input = client.submit_prompt("rules".into()).ok().unwrap();
    assert!(
        matches!(input,Input::Run(Action::Create {name,kind:Kind::Rule},Some(origin)) if name=="rules" && matches!(origin.0,Prompt::AddName) && origin.1=="rules")
    );
    assert!(client.prompt.is_none());
}
#[test]
fn close_all_submission_keeps_the_confirmed_instance_and_pid() {
    let mut client = Client::new(Kind::Proxy);
    client.prompt(Prompt::CloseAll {
        name: "original".into(),
        pid: Some(41),
    });
    client.active_profile = Some("replacement".into());
    let input = client.submit_prompt("yes".into()).ok().unwrap();
    assert!(
        matches!(input,Input::Run(Action::Runtime{name,expected_pid:Some(41),command:astra_model::runtime::Command::Disconnect(None)},_) if name=="original")
    );
}
#[test]
fn recovery_preserves_current_draft_instead_of_replaying_a_stale_save() {
    let mut client = Client::new(Kind::Proxy);
    client.editor = Some(crate::editor::Editor::new(
        "proxy".into(),
        "standalone=true\n".into(),
    ));
    client.fail(
        "permission denied",
        Resume::Request(Action::Save {
            name: "proxy".into(),
            original: "old".into(),
            text: "stale".into(),
        }),
    );
    assert!(matches!(
        client.recover(RecoveryAction::Continue),
        Input::Idle
    ));
    assert_eq!(client.editor.as_ref().unwrap().text(), "standalone=true\n");
    assert!(client.recovery.is_none());
}
#[test]
fn recovery_back_preserves_draft_and_stops_session_retries() {
    let mut client = Client::new(Kind::Proxy);
    client.editor = Some(crate::editor::Editor::new(
        "proxy".into(),
        "standalone=true\n".into(),
    ));
    client.fail("disk full", Resume::Session);
    assert!(matches!(client.recover(RecoveryAction::Back), Input::Idle));
    assert!(client.persistence_paused);
    assert!(client.editor.is_some());
    assert!(client.recovery.is_none());
}

#[test]
fn picker_submit_validates_before_dismissing_and_retains_the_draft_on_error() {
    let mut client = Client::new(Kind::Proxy);
    client.editor = Some(
        crate::editor::Editor::new("proxy".into(), "standalone=true\n".into())
            .with_kind(Kind::Proxy),
    );
    client
        .prepare_node_import(
            "[[outbounds]]\ntag='edge'\nkind='socks5'\nserver='127.0.0.1:1080'\n",
            crate::import::MergeMode::Append,
        )
        .unwrap();
    let original = client.editor.as_ref().unwrap().text();
    client.node_transfer(crate::node_transfer::TransferAction::ToggleAll);
    assert!(client.submit_prompt(String::new()).is_err());
    assert!(matches!(
        client.prompt,
        Some(Prompt::ImportNodeSelection { .. })
    ));
    assert!(client.recovery.is_none());
    assert_eq!(client.editor.as_ref().unwrap().text(), original);
    client.node_transfer(crate::node_transfer::TransferAction::ToggleIndex(0));
    assert!(matches!(
        client.submit_prompt(String::new()),
        Ok(Input::Idle)
    ));
    assert!(client.prompt.is_none());
    assert!(client.editor.as_ref().unwrap().text().contains("edge"));
}

#[test]
fn update_confirmation_requires_explicit_acceptance_of_the_captured_target() {
    let request = crate::protocol::updates::UpdateRequest {
        operation_id: "confirm-original".into(),
        intent: crate::protocol::updates::UpdateIntent::Activate {
            release_id: "original".into(),
        },
    };
    let mut client = Client::new(Kind::Proxy);
    client.prompt(Prompt::UpdateConfirm(request.clone()));
    assert!(matches!(
        client.submit_prompt(String::new()),
        Ok(Input::Idle)
    ));
    client.prompt(Prompt::UpdateConfirm(request));
    assert!(
        matches!(client.submit_prompt("yes".into()), Ok(Input::Run(Action::UpdateTransaction { request }, _)) if request.operation_id == "confirm-original" && matches!(&request.intent, crate::protocol::updates::UpdateIntent::Activate { release_id } if release_id == "original"))
    );
}
