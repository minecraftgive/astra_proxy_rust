use super::*;
use crate::protocol::{Instance, Row};
fn setup() -> Client {
    let mut ui = Client::from_session(Session::default());
    ui.rows = vec![
        Row {
            instance: Instance {
                name: "rule".into(),
                kind: Kind::Rule,
                config: "rule.toml".into(),
            },
            pid: Some(1),
            state: "Running".into(),
            uptime: 0,
            changed: false,
        },
        Row {
            instance: Instance {
                name: "edge".into(),
                kind: Kind::Proxy,
                config: "edge.toml".into(),
            },
            pid: Some(2),
            state: "Running".into(),
            uptime: 0,
            changed: false,
        },
    ];
    ui
}
#[test]
fn startup_selects_instance_after_state_arrives_and_does_not_act_on_a_replacement() {
    let mut ui = setup();
    let launch = Launch::detached(View::Logs, Some("rule".into()), Kind::Proxy);
    assert!(matches!(ui.apply_launch(&launch),Some(Action::Logs(name))if name=="rule"));
    assert_eq!(ui.kind, Kind::Rule);
    assert_eq!(ui.selected().unwrap().instance.name, "rule");
    let missing = Launch::detached(View::Diagnostics, Some("removed".into()), Kind::Proxy);
    assert!(ui.apply_launch(&missing).is_none());
    assert!(ui.message.contains("removed"));
}

#[test]
fn navigation_retains_draft_and_does_not_push_history_when_returning() {
    let mut client = setup();
    client.editor = Some(
        crate::editor::Editor::new("edge".into(), "standalone=true\n".into())
            .with_kind(Kind::Proxy),
    );
    client
        .editor
        .as_mut()
        .unwrap()
        .import("standalone=false\n".into())
        .unwrap();
    assert!(matches!(
        client.navigate(crate::navigation::PROXIES),
        Input::Idle
    ));
    assert!(client.editor.is_none());
    assert_eq!(client.drafts.len(), 1);
    client.navigate(crate::navigation::RULES);
    assert_eq!(client.navigation_stack, vec![0, 1]);
    client.navigate_back();
    assert_eq!(client.page, 1);
    assert_eq!(client.navigation_stack, vec![0]);
}
