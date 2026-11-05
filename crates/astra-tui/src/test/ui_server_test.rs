use super::*;
#[test]
fn server_selection_does_not_target_an_active_proxy() {
    let mut ui = Ui::new(Session::default());
    ui.kind = Kind::Server;
    ui.rows = vec![Row {
        instance: Instance {
            name: "edge".into(),
            kind: Kind::Server,
            config: "server.toml".into(),
        },
        pid: Some(7),
        state: "Running".into(),
        uptime: 1,
        changed: false,
    }];
    assert!(ui.active_proxy().is_none());
    assert_eq!(ui.selected().unwrap().instance.name, "edge");
    assert!(
        matches!(ui.server_key(event::KeyEvent::new(KeyCode::Char('v'),KeyModifiers::NONE)),Some(Input::Run(Action::Validate(name),_)) if name=="edge")
    );
}
#[test]
fn two_kind_session_selection_migrates_without_losing_state() {
    #[derive(Deserialize)]
    struct Old {
        #[serde(deserialize_with = "read_selections")]
        selections: [usize; 3],
    }
    assert_eq!(
        serde_json::from_str::<Old>(r#"{"selections":[2,3]}"#)
            .unwrap()
            .selections,
        [2, 3, 0]
    );
}
