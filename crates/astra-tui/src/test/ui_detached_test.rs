use super::*;
fn setup() -> Ui {
    let mut ui = Ui::new(Session::default());
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
fn shortcuts_open_pages_and_selected_logs_without_editing_or_navigation() {
    let mut ui = setup();
    ui.page = 2;
    let Input::OpenWindow(launch) = ui.key(event::KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE))
    else {
        panic!("no new window")
    };
    assert_eq!(launch.view, Some(View::Connections));
    assert_eq!(launch.instance.as_deref(), Some("edge"));
    ui.editor = Some(Editor::new("edge".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
    let draft = ui.editor.as_ref().unwrap().text();
    assert!(matches!(
        ui.key(event::KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        Input::Idle
    ));
    for (c, view) in [
        ('1', View::Dashboard),
        ('2', View::Proxies),
        ('3', View::Connections),
        ('4', View::Rules),
        ('5', View::Logs),
        ('6', View::Profiles),
    ] {
        assert!(
            matches!(ui.key(event::KeyEvent::new(KeyCode::Char(c),KeyModifiers::ALT)),Input::OpenWindow(launch) if launch.view==Some(view))
        );
    }
    assert_eq!(ui.page, 2);
    assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
    ui.editor = None;
    ui.viewer = Some(logs::Viewer::new("edge".into(), Default::default()));
    assert!(
        matches!(ui.key(event::KeyEvent::new(KeyCode::F(8),KeyModifiers::NONE)),Input::OpenWindow(launch)if launch.view==Some(View::Logs)&&launch.instance.as_deref()==Some("edge"))
    );
    ui.tui_viewer = Some(logs::Viewer::new("@tui".into(), Default::default()));
    assert!(
        matches!(ui.key(event::KeyEvent::new(KeyCode::F(8),KeyModifiers::NONE)),Input::OpenWindow(launch)if launch.view==Some(View::TuiLog))
    );
}
