use super::*;
fn key(c: char) -> event::KeyEvent {
    event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn sample_ui(running: bool) -> Ui {
    let mut ui = Ui::new(Session::default());
    // Instance-level shortcuts live on the Profiles page.
    ui.page = PROFILES;
    ui.rows = vec![
        Row {
            instance: Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: "/configs/proxy.toml".into(),
            },
            pid: running.then_some(42),
            state: if running { "Running" } else { "Stopped" }.into(),
            uptime: 610,
            changed: false,
        },
        Row {
            instance: Instance {
                name: "rules".into(),
                kind: Kind::Rule,
                config: "/configs/rules.toml".into(),
            },
            pid: None,
            state: "Stopped".into(),
            uptime: 0,
            changed: false,
        },
    ];
    ui.details_supported = true;
    ui.details.insert(
        "proxy".into(),
        InstanceDetails {
            modes: vec!["Rules".into(), "TUN".into()],
            listeners: vec!["SOCKS5 127.0.0.1:1080".into()],
            bindings: vec!["rules".into()],
            outbounds: vec!["edge".into()],
            log_level: LogLevel::Route,
            ..InstanceDetails::default()
        },
    );
    ui
}
fn press(ui: &mut Ui, code: KeyCode) -> Input {
    ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
}
fn import_ui() -> Ui {
    let mut ui = sample_ui(false);
    ui.editor =
        Some(Editor::new("proxy".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
    ui
}
#[test]
fn operation_status_defaults_to_latest_line() {
    let mut ui = sample_ui(true);
    ui.operation_steps = vec!["first".into(), "latest".into()];
    assert_eq!(ui.operation_text(), "latest");
    assert_eq!(ui.operation_text().lines().count(), 1);
    ui.message = "new result".into();
    ui.operation_result = Some("old result".into());
    assert_eq!(ui.operation_text(), "new result");
}
#[test]
fn help_is_context_specific_and_popups_fit_content_and_small_terminals() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let running = sample_ui(true).command_help();
        let stopped = sample_ui(false).command_help();
        assert!(running.contains("Disconnect profile") && !running.contains("Connect profile"));
        assert!(stopped.contains("Connect profile") && stopped.contains("Not running"));
        assert!(
            running.contains("F2")
                && running.contains("All other actions are in the command palette")
        );
        let mut ui = sample_ui(false);
        ui.help_override = Some("Context help\nSpecific action".into());
        let context = ui.command_help();
        assert_eq!(context, "Context help\nSpecific action");
        let area = ratatui::layout::Rect::new(0, 0, 200, 60);
        let popup = crate::help::popup(area, &context);
        assert!(popup.width < 76 && popup.height < 12 && popup.x > 0 && popup.y > 0);
        for (w, h) in [(10, 5), (30, 12), (80, 24)] {
            let area = ratatui::layout::Rect::new(0, 0, w, h);
            let popup = crate::help::popup(area, &running);
            assert!(popup.right() <= w && popup.bottom() <= h);
        }
    });
}
#[test]
fn help_overlay_preserves_state_and_does_not_capture_question_marks_in_inputs() {
    let mut ui = sample_ui(true);
    let selected = ui.selections;
    assert!(matches!(ui.key(key('?')), Input::Idle));
    assert_eq!(ui.help_scroll, Some(0));
    assert!(matches!(press(&mut ui, KeyCode::Down), Input::Idle));
    assert_eq!(ui.help_scroll, Some(1));
    assert_eq!(ui.selections, selected);
    ui.key(key('?'));
    assert!(ui.help_scroll.is_none());
    ui.prompt(Prompt::AddName);
    ui.key(key('?'));
    assert!(ui.help_scroll.is_none());
    assert_eq!(ui.input, "?");
    ui.prompt = None;
    ui.key(key('?'));
    use ratatui::{backend::TestBackend, Terminal};
    for language in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            for (width, height) in [(100, 30), (36, 12), (12, 7)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| ui.render(frame)).unwrap();
            }
        });
    }
    ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(ui.help_scroll.is_none());
}
#[test]
fn vim_list_navigation_and_log_shortcut_leave_prompt_text_intact() {
    let mut ui = sample_ui(false);
    ui.vim_keys = true;
    let mut second = ui.rows[0].clone();
    second.instance.name = "second".into();
    ui.rows.insert(1, second);
    ui.key(key('j'));
    assert_eq!(ui.selections[0], 1);
    ui.key(key('k'));
    assert_eq!(ui.selections[0], 0);
    ui.key(key('G'));
    assert_eq!(ui.selections[0], 1);
    ui.key(key('g'));
    ui.key(key('g'));
    assert_eq!(ui.selections[0], 0);
    press(&mut ui, KeyCode::Enter);
    ui.key(key('j'));
    assert_eq!(ui.menu, Some(1));
    ui.key(key('h'));
    assert!(ui.menu.is_none());
    ui.key(key('l'));
    assert_eq!(ui.kind, Kind::Rule);
    assert!(matches!(ui.key(key('L')),Input::Run(Action::Logs(name),_) if name=="rules"));
    ui.prompt(Prompt::AddName);
    for ch in "hjklgg".chars() {
        ui.key(key(ch));
    }
    assert_eq!(ui.input, "hjklgg");
}
#[test]
fn diagnostics_are_only_offered_for_proxy_instances_and_supported_managers() {
    let mut ui = sample_ui(false);
    ui.details.get_mut("proxy").unwrap().diagnostics_supported = true;
    assert!(ui
        .operations()
        .iter()
        .any(|op| matches!(op, Operation::Diagnostics)));
    assert!(
        matches!(ui.key(key('t')), Input::Run(Action::Diagnostics(name), None) if name == "proxy")
    );
    ui.details.get_mut("proxy").unwrap().diagnostics_supported = false;
    assert!(matches!(ui.key(key('t')), Input::Idle));
    assert_eq!(ui.message, "Restart manager to enable diagnostics");
    ui.kind = Kind::Rule;
    assert!(!ui
        .operations()
        .iter()
        .any(|op| matches!(op, Operation::Diagnostics)));
    assert!(matches!(ui.key(key('t')), Input::Idle));
}
#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_rule_query_keeps_draft_and_explains_rule_set_toggle() {
    let fixture = crate::import::tests::Fixture::new();
    let home = fixture.0.join("query-manager");
    let socket = home.join("management.sock");
    let bin = crate::manager::test_binaries();
    let manager = tokio::spawn(crate::manager::serve(home, bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let text = format!("socket={:?}\ndefault_action='direct'\n[[rules]]\nkind='rule_set'\nvalue='sites'\naction='block'\n[rule_sets.sites]\nbehavior='domain'\npayload=['+.example.test']\n", fixture.0.join("query-route.sock").to_string_lossy());
    let path = fixture.write("query-rules.toml", &text);
    call(
        &socket,
        Action::Add(Instance {
            name: "rules".into(),
            kind: Kind::Rule,
            config: path,
        }),
    )
    .await
    .unwrap();
    call(&socket, Action::Start("rules".into())).await.unwrap();
    let mut ui = Ui::new(Session::default());
    ui.kind = Kind::Rule;
    ui.page = PROFILES;
    ui.execute(&socket, Action::List, None).await;
    ui.key(key('t'));
    assert!(matches!(ui.prompt, Some(Prompt::Explain(_))));
    ui.paste("a.example.test:443 tcp");
    let Input::Run(action, origin) = press(&mut ui, KeyCode::Enter) else {
        panic!("query action");
    };
    ui.execute(&socket, action, origin.map(|origin| *origin))
        .await;
    let report = &ui.explanation.as_ref().unwrap().report;
    assert_eq!(report.action, astra_model::RouteAction::Block);
    assert!(report.path.starts_with("rules[1] -> rule_set(\"sites\")"));
    assert_eq!(
        report.condition,
        Some(("domain_suffix".into(), "example.test".into()))
    );
    let previous_revision = report.revision;
    press(&mut ui, KeyCode::Esc);
    let draft = text.replace("default_action='direct'", "default_action='proxy:unsaved'");
    ui.editor = Some(Editor::new("rules".into(), draft.clone()).with_kind(Kind::Rule));
    ui.key(event::KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    ));
    ui.paste("unmatched.test");
    let Input::Run(action, origin) = press(&mut ui, KeyCode::Enter) else {
        panic!("draft query");
    };
    ui.execute(&socket, action, origin.map(|origin| *origin))
        .await;
    assert_eq!(
        ui.explanation.as_ref().unwrap().report.default_action,
        "direct"
    );
    assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
    press(&mut ui, KeyCode::Esc);
    call(
        &socket,
        Action::Save {
            name: "rules".into(),
            original: text.clone(),
            text: text.replace("behavior='domain'", "enabled=false\nbehavior='domain'"),
        },
    )
    .await
    .unwrap();
    let reply = call(
        &socket,
        Action::ExplainRoute {
            name: "rules".into(),
            request: crate::explanation::request("a.example.test").unwrap(),
        },
    )
    .await
    .unwrap();
    let report: astra_model::control::RouteExplanation = serde_json::from_str(&reply.text).unwrap();
    assert_eq!(report.action, astra_model::RouteAction::Direct);
    assert_eq!(report.path, "default_action");
    assert!(report.revision > previous_revision);
    call(&socket, Action::Shutdown).await.unwrap();
    manager.await.unwrap().unwrap();
}
#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_rule_editor_completes_a_bound_proxy_node_and_saves_valid_action() {
    let fixture = crate::import::tests::Fixture::new();
    let home = fixture.0.join("manager");
    let socket = home.join("management.sock");
    let bin = crate::manager::test_binaries();
    let manager = tokio::spawn(crate::manager::serve(home, bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    call(
        &socket,
        Action::Create {
            name: "rules".into(),
            kind: Kind::Rule,
        },
    )
    .await
    .unwrap();
    let path = fixture.write("proxy.toml", "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='east-node'\nserver='127.0.0.1:1081'\nusername='test-user'\npassword='do-not-show'\n");
    call(
        &socket,
        Action::Add(Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: path,
        }),
    )
    .await
    .unwrap();
    call(
        &socket,
        Action::BindStart {
            name: "proxy".into(),
            targets: vec!["rules".into()],
        },
    )
    .await
    .unwrap();
    let mut ui = Ui::new(Session::default());
    ui.kind = Kind::Rule;
    ui.execute(&socket, Action::List, None).await;
    ui.execute(&socket, Action::Read("rules".into()), None)
        .await;
    assert!(ui.recovery.is_none());
    let editor = ui.editor.as_mut().unwrap();
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .unwrap();
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    editor.paste("eas");
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )))
        .unwrap();
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    let text = editor.text();
    assert!(!text.contains("do-not-show"));
    assert_eq!(
        toml::from_str::<toml::Value>(&text).unwrap()["default_action"].as_str(),
        Some("proxy:east-node")
    );
    let Input::Run(save, _) = ui.key(event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )) else {
        panic!("save");
    };
    ui.execute(&socket, save, None).await;
    assert!(ui.recovery.is_none());
    let saved = call(&socket, Action::Read("rules".into()))
        .await
        .unwrap()
        .text;
    assert_eq!(
        toml::from_str::<toml::Value>(&saved).unwrap()["default_action"].as_str(),
        Some("proxy:east-node")
    );
    call(&socket, Action::Shutdown).await.unwrap();
    manager.await.unwrap().unwrap();
}
#[test]
fn profiles_and_rules_pages_search() {
    let mut ui = sample_ui(true);
    ui.key(key('/'));
    for c in "rules".chars() {
        ui.key(key(c));
    }
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.kind, Kind::Rule);
    assert_eq!(ui.selected().unwrap().instance.name, "rules");
    // The previous query is prefilled; Enter wraps back through both lists.
    ui.key(event::KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::CONTROL,
    ));
    assert_eq!(ui.input, "rules");
    ui.key(key('p'));
    ui.key(key('r'));
    ui.key(key('o'));
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.selected().unwrap().instance.name, "proxy");
    // On Rules, / opens the profile's rule core in the editor with its search running.
    ui.kind = Kind::Proxy;
    ui.navigate(shell::RULES);
    assert!(matches!(ui.key(key('/')), Input::Run(Action::Read(name), _) if name == "rules"));
    ui.editor = Some(
        Editor::new(
            "rules".into(),
            include_str!("../../../../configs/rule.toml").into(),
        )
        .with_kind(Kind::Rule),
    );
    ui.apply_pending_search();
    assert!(!ui.editor.as_ref().unwrap().browsing());
    assert!(!ui.search_on_open);
}
#[test]
fn editor_search_takes_every_key_until_closed() {
    let mut ui = sample_ui(false);
    let draft = include_str!("../../../../configs/rule.toml");
    ui.editor = Some(Editor::new("rules".into(), draft.into()).with_kind(Kind::Rule));
    for open in [
        key('/'),
        event::KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
    ] {
        ui.key(open);
        assert!(!ui.editor.as_ref().unwrap().browsing());
        // Page numbers, : and q are query text here, not shortcuts.
        for c in "6:q".chars() {
            assert!(matches!(ui.key(key(c)), Input::Idle));
        }
        assert_eq!(ui.page, PROFILES);
        assert!(ui.palette.is_none());
        press(&mut ui, KeyCode::Esc);
        assert!(ui.editor.as_ref().unwrap().browsing());
    }
    ui.key(key('/'));
    for c in "blocked.test".chars() {
        ui.key(key(c));
    }
    press(&mut ui, KeyCode::Enter);
    assert!(ui.editor.as_ref().unwrap().browsing());
    assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
}
#[test]
fn dropped_config_and_import_shortcut_replace_only_draft_then_use_normal_save() {
    let fixture = crate::import::tests::Fixture::new();
    let path = fixture.write(
        "中文 file.toml",
        "standalone=true\nsocks5_listen='127.0.0.1:1234'\n",
    );
    for shortcut in [false, true] {
        let mut ui = import_ui();
        if shortcut {
            ui.key(event::KeyEvent::new(
                KeyCode::Char('o'),
                KeyModifiers::CONTROL,
            ));
            assert!(matches!(ui.prompt, Some(Prompt::Import)));
            ui.paste(&format!("\"{}\"", path.display()));
            press(&mut ui, KeyCode::Enter);
        } else {
            ui.paste(&format!("\"{}\"", path.display()));
        }
        assert!(ui.prompt.is_none());
        assert!(ui.editor.as_ref().unwrap().text().contains("1234"));
        assert_eq!(ui.editor.as_ref().unwrap().original, "standalone=true\n");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "standalone=true\nsocks5_listen='127.0.0.1:1234'\n"
        );
        assert!(
            matches!(ui.key(event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)), Input::Run(Action::Save {name, original, text}, None) if name == "proxy" && original == "standalone=true\n" && text.contains("1234"))
        );
    }
}
#[test]
fn import_failure_cancel_and_confirmation_preserve_draft_and_survive_restore() {
    let fixture = crate::import::tests::Fixture::new();
    let path = fixture.write(
        "new.toml",
        "standalone=true\nhttp_listen='127.0.0.1:9876'\n",
    );
    let mut ui = import_ui();
    ui.editor
        .as_mut()
        .unwrap()
        .import("standalone=true\n# my draft\n".into())
        .unwrap();
    let old = ui.editor.as_ref().unwrap().text();
    ui.paste("missing-file.toml");
    assert!(ui.recovery.is_some());
    assert_eq!(ui.editor.as_ref().unwrap().text(), old);
    press(&mut ui, KeyCode::Char('c'));
    assert!(matches!(ui.prompt, Some(Prompt::Import)));
    press(&mut ui, KeyCode::Esc);
    ui.paste(path.to_str().unwrap());
    assert!(matches!(ui.prompt, Some(Prompt::ImportConfirm { .. })));
    ui.paste("ignored.toml");
    assert_eq!(ui.editor.as_ref().unwrap().text(), old);
    press(&mut ui, KeyCode::Esc);
    assert_eq!(ui.editor.as_ref().unwrap().text(), old);
    ui.paste(path.to_str().unwrap());
    let session_path = fixture.0.join("ui-session.json");
    ui.session.save(&session_path).unwrap();
    let mut restored = Ui::new(Session::load(&session_path).unwrap());
    assert!(matches!(
        restored.prompt,
        Some(Prompt::ImportConfirm { .. })
    ));
    press(&mut restored, KeyCode::Tab);
    press(&mut restored, KeyCode::Enter);
    assert!(restored.prompt.is_none());
    assert!(restored.editor.as_ref().unwrap().text().contains("9876"));
    assert_eq!(
        restored.editor.as_ref().unwrap().original,
        "standalone=true\n"
    );
}
#[test]
fn pasting_inside_a_field_keeps_field_editing_and_ctrl_o_does_not_discard_input() {
    let mut ui = import_ui();
    // The first row is a boolean that toggles in place; open the next, text-like field.
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Enter);
    assert!(!ui.editor.as_ref().unwrap().browsing());
    ui.key(event::KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    ));
    assert!(ui.prompt.is_none());
    ui.paste("true");
    assert!(ui.recovery.is_none());
    assert!(!ui.editor.as_ref().unwrap().browsing());
}
#[test]
fn yaml_import_review_renders_in_three_languages_and_small_windows() {
    use ratatui::backend::TestBackend;
    let fixture = crate::import::tests::Fixture::new();
    let path = fixture.write("source.yaml", "socks-port: 1088\nrules: ['MATCH,DIRECT']\n");
    let mut ui = import_ui();
    ui.paste(path.to_str().unwrap());
    assert!(matches!(ui.prompt, Some(Prompt::ImportConfirm { .. })));
    for lang in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(lang, || {
            for (width, height) in [(110, 32), (60, 16), (20, 8)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| ui.render(frame)).unwrap();
            }
        });
    }
    press(&mut ui, KeyCode::Tab);
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.editor.as_ref().unwrap().original, "standalone=true\n");
    assert!(ui.editor.as_ref().unwrap().text().contains("1088"));
}
#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_import_saves_each_core_without_changing_source_or_binding() {
    let fixture = crate::import::tests::Fixture::new();
    let home = fixture.0.join("manager");
    let socket = home.join("management.sock");
    let bin = crate::manager::test_binaries();
    let manager = tokio::spawn(crate::manager::serve(home, bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let source = "socks-port: 11088\nrules: ['MATCH,DIRECT']\n";
    let path = fixture.write("导入 file.yaml", source);
    let mut ui = Ui::new(Session::default());
    for (name, kind) in [("rules", Kind::Rule), ("proxy", Kind::Proxy)] {
        ui.kind = kind;
        ui.execute(
            &socket,
            Action::Create {
                name: name.into(),
                kind,
            },
            None,
        )
        .await;
        assert!(ui.recovery.is_none());
        let original = ui.editor.as_ref().unwrap().original.clone();
        ui.execute(&socket, Action::List, None).await;
        let bound = ui
            .rows
            .iter()
            .find(|r| r.instance.name == name)
            .unwrap()
            .instance
            .config
            .clone();
        ui.paste(path.to_str().unwrap());
        assert!(matches!(ui.prompt, Some(Prompt::ImportConfirm { .. })));
        assert!(
            !ui.confirmation_accept,
            "each import starts with Cancel selected"
        );
        press(&mut ui, KeyCode::Tab);
        press(&mut ui, KeyCode::Enter);
        assert_eq!(std::fs::read_to_string(&bound).unwrap(), original);
        let Input::Run(action, _) = ui.key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        )) else {
            panic!("save");
        };
        ui.execute(&socket, action, None).await;
        assert!(ui.recovery.is_none());
        let saved = std::fs::read_to_string(&bound).unwrap();
        assert_ne!(saved, original);
        ui.execute(&socket, Action::List, None).await;
        assert_eq!(
            ui.rows
                .iter()
                .find(|r| r.instance.name == name)
                .unwrap()
                .instance
                .config,
            bound
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
    call(&socket, Action::Shutdown).await.unwrap();
    manager.await.unwrap().unwrap();
}
#[test]
fn context_menu_only_offers_applicable_actions_and_keeps_shortcuts() {
    let mut ui = sample_ui(true);
    assert!(!ui
        .operations()
        .iter()
        .any(|operation| matches!(operation, Operation::Start | Operation::Remove)));
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.menu, Some(0));
    for _ in 0..3 {
        press(&mut ui, KeyCode::Down);
    }
    assert!(
        matches!(press(&mut ui, KeyCode::Enter), Input::Run(Action::Read(name), None) if name == "proxy")
    );
    assert!(matches!(ui.key(key('d')), Input::Idle));
    assert!(ui.prompt.is_none());
    let mut ui = sample_ui(false);
    assert!(ui
        .operations()
        .iter()
        .any(|operation| matches!(operation, Operation::UpdateSubscriptions)));
    assert!(!ui.operations().iter().any(|operation| matches!(
        operation,
        Operation::Stop | Operation::Restart | Operation::Reload
    )));
    // Restart is available through the command palette, never a capital-letter default.
    assert!(matches!(ui.key(key('r')), Input::Idle));
    assert!(matches!(ui.key(key('R')), Input::Idle));
    assert!(
        matches!(ui.operation(Operation::Restart), Input::Run(Action::Start(name), None) if name == "proxy")
    );
    assert!(matches!(ui.key(key('c')), Input::Run(Action::Start(name), None) if name == "proxy"));
}
#[test]
fn bindings_are_selected_without_typing_and_cancel_does_not_save() {
    let mut ui = sample_ui(false);
    ui.key(key('b'));
    assert_eq!(ui.binding_picker.as_ref().unwrap().targets, ["rules"]);
    press(&mut ui, KeyCode::Char(' '));
    assert!(ui.binding_picker.as_ref().unwrap().targets.is_empty());
    assert!(
        matches!(press(&mut ui, KeyCode::Enter), Input::Run(Action::BindStart { name, targets }, None) if name == "proxy" && targets.is_empty())
    );
    ui.key(key('b'));
    press(&mut ui, KeyCode::Char(' '));
    press(&mut ui, KeyCode::Esc);
    assert!(ui.binding_picker.is_none());
    assert_eq!(ui.details["proxy"].bindings, ["rules"]);
}
#[test]
fn force_start_requires_confirmation_and_is_only_offered_for_stopped_cores() {
    let mut ui = sample_ui(false);
    ui.rows[0].state = "Unresponsive or unverified core".into();
    assert!(ui
        .operations()
        .iter()
        .any(|op| matches!(op, Operation::ForceStart)));
    assert!(matches!(ui.operation(Operation::ForceStart), Input::Idle));
    assert!(matches!(&ui.prompt, Some(Prompt::ForceStart(name)) if name == "proxy"));
    assert!(!ui.confirmation_accept);
    assert!(matches!(press(&mut ui, KeyCode::Enter), Input::Idle));
    assert!(ui.prompt.is_none());

    ui.operation(Operation::ForceStart);
    press(&mut ui, KeyCode::Tab);
    assert!(matches!(
        press(&mut ui, KeyCode::Enter),
        Input::Run(Action::ForceStart(name), Some(origin))
            if name == "proxy" && matches!(&origin.0, Prompt::ForceStart(_))
    ));
    ui.operation(Operation::ForceStart);
    assert!(
        !ui.confirmation_accept,
        "Each replacement requires fresh confirmation"
    );
    press(&mut ui, KeyCode::Esc);
    assert!(ui.prompt.is_none());

    ui.kind = Kind::Rule;
    assert!(ui
        .operations()
        .iter()
        .any(|op| matches!(op, Operation::ForceStart)));
    let mut ui = sample_ui(true);
    assert!(!ui
        .operations()
        .iter()
        .any(|op| matches!(op, Operation::ForceStart)));
    assert!(matches!(ui.operation(Operation::ForceStart), Input::Idle));
    assert!(ui.prompt.is_none());
    ui.rows[0].pid = None;
    ui.rows[0].instance.kind = Kind::Server;
    ui.kind = Kind::Server;
    assert!(!ui
        .operations()
        .iter()
        .any(|op| matches!(op, Operation::ForceStart)));
    assert!(matches!(ui.operation(Operation::ForceStart), Input::Idle));
    assert!(ui.prompt.is_none());
}

#[test]
fn force_start_confirmation_explains_replacement_and_retained_processes() {
    crate::i18n::with_language(crate::i18n::Language::Simplified, || {
        let mut ui = sample_ui(false);
        ui.operation(Operation::ForceStart);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let screen = screen.replace(' ', "");
        assert!(screen.contains("强制启动并覆盖旧会话"));
        assert!(screen.contains("proxy"));
        assert!(screen.contains("旧会话记录"));
        assert!(screen.contains("旧进程可能仍在运行"));
    });
}
#[test]
fn binding_picker_selects_one_rule_or_multiple_proxies() {
    let mut ui = sample_ui(false);
    let mut other_proxy = ui.rows[0].clone();
    other_proxy.instance.name = "other-proxy".into();
    ui.rows.push(other_proxy);
    let mut other_rule = ui
        .rows
        .iter()
        .find(|row| row.instance.kind == Kind::Rule)
        .unwrap()
        .clone();
    other_rule.instance.name = "other-rule".into();
    ui.rows.push(other_rule);
    ui.key(key('b'));
    assert_eq!(ui.binding_picker.as_ref().unwrap().choices.len(), 2);
    assert!(ui
        .binding_picker
        .as_ref()
        .unwrap()
        .choices
        .iter()
        .all(|(_, kind)| *kind == Kind::Rule));
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Char(' '));
    assert_eq!(ui.binding_picker.as_ref().unwrap().targets, ["other-rule"]);
    press(&mut ui, KeyCode::Esc);
    ui.kind = Kind::Rule;
    assert!(ui
        .operations()
        .iter()
        .any(|operation| matches!(operation, Operation::Bind)));
    ui.key(key('b'));
    assert!(ui
        .binding_picker
        .as_ref()
        .unwrap()
        .choices
        .iter()
        .all(|(_, kind)| *kind == Kind::Proxy));
    press(&mut ui, KeyCode::Char(' '));
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Char(' '));
    assert_eq!(
        ui.binding_picker.as_ref().unwrap().targets,
        ["proxy", "other-proxy"]
    );
}
#[test]
fn confirmations_default_to_cancel_and_require_fresh_explicit_selection() {
    let mut ui = sample_ui(false);
    ui.key(key('d'));
    assert!(matches!(press(&mut ui, KeyCode::Enter), Input::Idle));
    assert!(ui.prompt.is_none());
    ui.key(key('d'));
    press(&mut ui, KeyCode::Tab);
    let mut repeated = event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    repeated.kind = KeyEventKind::Repeat;
    assert!(matches!(ui.key(repeated), Input::Idle));
    assert!(ui.prompt.is_some());
    assert!(
        matches!(press(&mut ui, KeyCode::Enter), Input::Run(Action::Remove(name), _) if name == "proxy")
    );
    ui.key(key('Q'));
    assert!(ui.prompt.is_none());
    ui.run_command(commands::Id::Shutdown);
    press(&mut ui, KeyCode::Esc);
    assert!(ui.prompt.is_none());
    ui.run_command(commands::Id::Shutdown);
    assert!(matches!(ui.key(key('y')), Input::Idle));
    press(&mut ui, KeyCode::Right);
    assert!(matches!(
        press(&mut ui, KeyCode::Enter),
        Input::Run(Action::Shutdown, _)
    ));
}
#[test]
fn configuration_paths_are_prefilled_and_paste_replaces_the_default() {
    let mut ui = sample_ui(false);
    // The path dialog moved to the action menu and command palette; c connects.
    ui.operation(Operation::Config);
    assert!(ui.input.ends_with("proxy.toml"));
    ui.paste_prompt("new path.toml");
    assert_eq!(ui.input, "new path.toml");
    press(&mut ui, KeyCode::Esc);
    ui.key(key('y'));
    assert!(ui.input.ends_with("proxy-copy.toml"));
    ui.key(event::KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    ));
    assert!(ui.input.is_empty());
}
#[test]
fn instance_overview_and_overlays_render_in_all_languages_and_small_windows() {
    use crate::i18n::{with_language, Language};
    use ratatui::backend::TestBackend;
    for language in [
        Language::English,
        Language::Simplified,
        Language::Traditional,
    ] {
        with_language(language, || {
            let mut ui = sample_ui(true);
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal.draw(|frame| ui.render(frame)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            for value in [
                "proxy",
                "TUN",
                "127.0.0.1:1080",
                "rules",
                "PID 42",
                "00:10:10",
                "ROUTE",
                "edge",
            ] {
                assert!(text.contains(value), "Missing {value}: {text}");
            }
            ui.key(key('b'));
            for (width, height) in [(100, 24), (20, 8), (8, 4)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| ui.render(frame)).unwrap();
            }
            press(&mut ui, KeyCode::Esc);
            press(&mut ui, KeyCode::Enter);
            terminal.draw(|frame| ui.render(frame)).unwrap();
        });
    }
}
#[test]
fn older_manager_rows_work_without_sending_an_unsupported_binding_action() {
    let mut ui = sample_ui(false);
    let rows = ui.rows.clone();
    let mut reply = Reply::text("");
    reply.rows = rows;
    ui.update_rows(reply);
    assert!(!ui.details_supported);
    assert!(matches!(ui.key(key('b')), Input::Idle));
    assert!(ui.binding_picker.is_none());
    assert!(!ui.message.is_empty());
}
#[test]
fn new_instance_name_creates_bound_config_without_a_second_path_prompt() {
    let mut ui = Ui::new(Session::default());
    ui.kind = Kind::Rule;
    ui.prompt(Prompt::AddName);
    ui.input = "rules".into();
    let Input::Run(Action::Create { name, kind }, Some(origin)) =
        ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("create action")
    };
    let (Prompt::AddName, input) = *origin else {
        panic!("create origin")
    };
    assert_eq!(name, "rules");
    assert_eq!(input, "rules");
    assert_eq!(kind, Kind::Rule);
    assert!(ui.prompt.is_none());
}
#[test]
fn retry_restores_path_step_name_input_and_core() {
    let mut ui = Ui::new(Session::default());
    ui.kind = Kind::Rule;
    ui.fail(
        "permission denied",
        Resume::Prompt(
            Prompt::AddPath("rules".into()),
            "/private/rules.toml".into(),
        ),
    );
    assert!(matches!(ui.key(key('c')), Input::Idle));
    assert!(matches!(&ui.prompt, Some(Prompt::AddPath(name)) if name == "rules"));
    assert_eq!(ui.input, "/private/rules.toml");
    assert_eq!(ui.kind, Kind::Rule);
    assert!(ui.recovery.is_none());
}
#[tokio::test]
async fn save_conflicts_read_disk_once_and_keep_original_and_draft() {
    for (index, error) in [
        "File changed on disk; reopen before editing",
        "File changed during validation; reopen before editing",
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = crate::import::tests::Fixture::new();
        let socket = fixture.0.join(format!("conflict-{index}.sock"));
        let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
        let disk = "standalone=true\n# external edit\n";
        let server = tokio::spawn(async move {
            use astra_management::v2::*;
            let (stream, request) =
                test_rpc::save_request(&listener, "standalone=true\n", "standalone=false\n").await;
            test_rpc::error(stream, request, error).await;
            let (stream, request) = test_rpc::next(&listener).await;
            assert!(
                matches!(&request.body,RequestBody::Query(Query::Document{instance}) if instance=="proxy")
            );
            test_rpc::reply(
                stream,
                request,
                ResponseBody::Document(Document {
                    instance: "proxy".into(),
                    path: "proxy.toml".into(),
                    text: disk.into(),
                    revision: "disk-revision".into(),
                    kind: InstanceKind::Proxy,
                }),
            )
            .await;
        });
        let original = "standalone=true\n";
        let changed = "standalone=false\n";
        let mut ui = Ui::new(Session::default());
        ui.editor = Some(Editor::new("proxy".into(), original.into()).with_kind(Kind::Proxy));
        ui.editor.as_mut().unwrap().import(changed.into()).unwrap();
        tokio::time::timeout(
            Duration::from_secs(3),
            ui.execute(
                &socket,
                Action::Save {
                    name: "proxy".into(),
                    original: original.into(),
                    text: changed.into(),
                },
                None,
            ),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(ui.editor.as_ref().unwrap().text(), changed);
        assert_eq!(ui.editor.as_ref().unwrap().original, original);
        assert!(ui.recovery.as_ref().unwrap().message.contains(error));
        let observed = ui.drafts.values().next().unwrap();
        assert!(observed.conflict);
        assert_eq!(observed.observed_disk.as_deref(), Some(disk));
        ui.recover_key(KeyCode::Esc);
        ui.compare_draft();
        assert!(ui
            .help_override
            .as_ref()
            .unwrap()
            .contains("- # external edit"));
        astra_platform::platform::remove_socket(&socket);
    }
}
#[test]
fn retry_save_preserves_form_draft_and_b_ends_only_operation() {
    let mut ui = Ui::new(Session::default());
    let mut editor = Editor::new(
        "rules".into(),
        r#"socket = "/tmp/original.sock"
default_action = "direct"
"#
        .into(),
    );
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    editor.paste("/tmp/new.sock");
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    let draft = editor.text();
    let snapshot = serde_json::to_value(&editor).unwrap();
    let action = Action::Save {
        name: "rules".into(),
        original: editor.original.clone(),
        text: draft.clone(),
    };
    ui.editor = Some(editor);
    ui.fail("read only", Resume::Request(action.clone()));
    ui.key(key('c'));
    assert_eq!(
        serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
        snapshot
    );
    ui.fail("still read only", Resume::Request(action));
    ui.key(key('b'));
    assert!(ui.recovery.is_none());
    assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
}
#[test]
fn invalid_entry_uses_c_to_resume_and_b_to_cancel_the_field_only() {
    let mut ui = Ui::new(Session::default());
    let mut editor = Editor::new(
        "proxy".into(),
        include_str!("../../../../configs/proxy.toml").into(),
    );
    // Select IPC connections after standalone settings, socket, listeners and authentication.
    for _ in 0..7 {
        editor
            .key(crate::input::key(event::KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )))
            .unwrap();
    }
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    editor.paste("0");
    let snapshot = serde_json::to_value(&editor).unwrap();
    let original = editor.text();
    ui.editor = Some(editor);
    ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        ui.recovery.as_ref().unwrap().resume,
        Resume::Editor
    ));
    ui.key(key('c'));
    assert_eq!(
        serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
        snapshot
    );
    ui.key(event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    assert!(ui.recovery.is_some());
    ui.key(key('b'));
    assert!(ui.recovery.is_none());
    assert_eq!(ui.editor.as_ref().unwrap().text(), original);
    // A subsequent Enter reopens the unchanged field with its original value.
    ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    ui.editor.as_mut().unwrap().paste("16");
    ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(ui
        .editor
        .as_ref()
        .unwrap()
        .text()
        .contains("ipc_connections = 16"));
}
#[test]
fn external_editor_roundtrip_keeps_original_and_recovers_invalid_or_conflicting_edits() {
    let home = std::env::temp_dir().join(format!("astra-external-ui-{}", std::process::id()));
    astra_platform::storage::private_dir(&home).unwrap();
    let mut ui = sample_ui(false);
    let original = "standalone=true\nsocks5_listen='127.0.0.1:1080'\n";
    ui.editor = Some(Editor::new("proxy".into(), original.into()));
    assert!(matches!(
        ui.run_command(commands::Id::ExternalEditor),
        Input::ExternalEditor
    ));
    let (draft, _) = ui.prepare_external(&home).unwrap();
    let changed = original.replace("1080", "1088");
    std::fs::write(&draft.path, &changed).unwrap();
    let session = serde_json::to_string(&ui.session).unwrap();
    let mut ui = Ui::new(serde_json::from_str(&session).unwrap());
    ui.run_command(commands::Id::ReadExternal);
    assert_eq!(ui.editor.as_ref().unwrap().text(), changed);
    assert_eq!(ui.editor.as_ref().unwrap().original, original);
    assert!(
        matches!(ui.key(event::KeyEvent::new(KeyCode::Char('s'),KeyModifiers::CONTROL)),Input::Run(Action::Save{original:base,text,..},_) if base==original && text==changed)
    );
    std::fs::write(&draft.path, "bad = [").unwrap();
    assert!(ui.read_external().is_err());
    assert_eq!(ui.editor.as_ref().unwrap().text(), changed);
    // Local edits made after the last external import cannot be silently replaced.
    ui.editor
        .as_mut()
        .unwrap()
        .import(changed.replace("1088", "1099"))
        .unwrap();
    std::fs::write(&draft.path, original).unwrap();
    assert!(ui.read_external().is_err());
    assert!(ui.editor.as_ref().unwrap().text().contains("1099"));
    assert!(ui.prepare_external(&home).is_err());
    assert_eq!(ui.external_draft.as_ref().unwrap().path, draft.path);

    std::fs::remove_file(draft.path).unwrap();
    std::fs::remove_dir(home.join("external-drafts")).unwrap();
    std::fs::remove_dir(home).unwrap();
}
#[test]
fn occupied_start_retries_same_action_and_cancel_does_not_execute() {
    let mut ui = Ui::new(Session::default());
    ui.fail(
        "address in use",
        Resume::Request(Action::Start("proxy".into())),
    );
    assert!(matches!(ui.key(key('c')), Input::Run(Action::Start(name), None) if name == "proxy"));
    ui.fail(
        "address in use",
        Resume::Request(Action::Start("proxy".into())),
    );
    assert!(matches!(ui.key(key('b')), Input::Idle));
    assert!(ui.recovery.is_none());
}
#[test]
fn bad_local_input_is_recoverable() {
    let mut ui = Ui::new(Session::default());
    ui.prompt(Prompt::Config("proxy".into()));
    ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(ui.recovery.is_some());
    ui.key(key('c'));
    assert!(matches!(&ui.prompt, Some(Prompt::Config(name)) if name == "proxy"));
}
#[test]
fn session_roundtrip_retains_draft_recovery_and_selection() {
    let mut session = Session {
        kind: Kind::Rule,
        selections: [2, 3, 0],
        editor: Some(Editor::new(
            "rules".into(),
            r#"socket = "/tmp/a.sock"
"#
            .into(),
        )),
        prompt: Some(Prompt::AddPath("other".into())),
        input: "/tmp/a b.toml".into(),
        recovery: Some(Recovery {
            message: "permission denied".into(),
            resume: Resume::Request(Action::Read("rules".into())),
        }),
        working_dir: launch_directory(),
        log_settings: HashMap::new(),
        active_log: None,
        external_draft: None,
        ..Session::default()
    };
    session
        .editor
        .as_mut()
        .unwrap()
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    session.editor.as_mut().unwrap().paste("/tmp/pending.sock");
    let form = serde_json::to_value(session.editor.as_ref().unwrap()).unwrap();
    let restored: Session = serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
    assert_eq!(restored.kind, Kind::Rule);
    assert_eq!(restored.selections, [2, 3, 0]);
    assert_eq!(restored.input, "/tmp/a b.toml");
    assert_eq!(
        serde_json::to_value(restored.editor.unwrap()).unwrap(),
        form
    );
    assert!(restored.recovery.is_some());
}
#[tokio::test]
#[ignore = "run after cargo build --workspace"]
async fn live_recovery_and_defaults() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!("astra-ui-recover-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let home = directory.join("state");
    let socket = home.join("management.sock");
    let bin = crate::manager::test_binaries();
    let manager = tokio::spawn(crate::manager::serve(home.clone(), bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut ui = Ui::new(Session::default());
    ui.kind = Kind::Rule;
    let path = directory.join("new/rule.toml");
    let instance = Instance {
        name: "rules".into(),
        kind: Kind::Rule,
        config: path.clone(),
    };
    ui.execute(
        &socket,
        Action::Add(instance),
        Some((Prompt::AddPath("rules".into()), path.display().to_string())),
    )
    .await;
    assert!(path.is_file());
    assert!(ui.recovery.is_none());
    assert_eq!(
        ui.editor.as_ref().unwrap().text(),
        std::fs::read_to_string(&path).unwrap()
    );
    // An invalid route action is rejected by the core; all form state survives c.
    let editor = ui.editor.as_mut().unwrap();
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .unwrap();
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    editor.paste("invalid-route");
    editor
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    let snapshot = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
    let Input::Run(save, _) = ui.key(event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )) else {
        panic!("save action");
    };
    ui.execute(&socket, save, None).await;
    assert!(ui.recovery.is_some());
    ui.key(key('c'));
    assert_eq!(
        serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
        snapshot
    );
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("invalid-route"));
    ui.editor = None;
    // A removed config during start is recreated and opened, never started silently.
    std::fs::remove_file(&path).unwrap();
    ui.execute(&socket, Action::Start("rules".into()), None)
        .await;
    assert!(ui.editor.is_some());
    assert!(ui.recovery.is_none());
    assert!(call(&socket, Action::List).await.unwrap().rows[0]
        .pid
        .is_none());
    // Path occupied by a directory: c restores the path prompt and instance name.
    let blocked = directory.join("occupied.toml");
    std::fs::create_dir(&blocked).unwrap();
    ui.editor = None;
    ui.execute(
        &socket,
        Action::SetConfig("rules".into(), blocked.clone()),
        Some((
            Prompt::Config("rules".into()),
            blocked.display().to_string(),
        )),
    )
    .await;
    assert!(ui.recovery.is_some());
    ui.key(key('c'));
    assert_eq!(ui.input, blocked.display().to_string());
    assert!(matches!(&ui.prompt, Some(Prompt::Config(name)) if name == "rules"));
    ui.prompt = None;
    // Repair a real filesystem error and retry without restarting the UI.
    let locked = directory.join("locked");
    #[cfg(unix)]
    {
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    }
    #[cfg(windows)]
    std::fs::write(&locked, "parent occupied by file").unwrap();
    let locked_path = locked.join("rule.toml");
    ui.execute(
        &socket,
        Action::SetConfig("rules".into(), locked_path.clone()),
        Some((
            Prompt::Config("rules".into()),
            locked_path.display().to_string(),
        )),
    )
    .await;
    #[cfg(unix)]
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    #[cfg(windows)]
    {
        std::fs::remove_file(&locked).unwrap();
        std::fs::create_dir(&locked).unwrap();
    }
    assert!(ui.recovery.is_some());
    ui.key(key('c'));
    let Input::Run(retry, origin) =
        ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("retry action");
    };
    ui.execute(&socket, retry, origin.map(|origin| *origin))
        .await;
    assert!(ui.recovery.is_none());
    assert!(ui.editor.is_some());
    assert!(locked_path.is_file());
    // Draft + cursor + recovery survive a real file roundtrip with private permissions.
    ui.editor
        .as_mut()
        .unwrap()
        .key(crate::input::key(event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    ui.editor
        .as_mut()
        .unwrap()
        .paste("/tmp/remembered-field.sock");
    ui.fail(
        "recoverable error",
        Resume::Request(Action::Validate("rules".into())),
    );
    let session_path = home.join("ui-session.json");
    assert!(ui.checkpoint(&session_path));
    let restored = Session::load(&session_path).unwrap();
    assert_eq!(
        restored.editor.as_ref().unwrap().text(),
        ui.editor.as_ref().unwrap().text()
    );
    assert!(restored.recovery.is_some());
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&session_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    call(&socket, Action::Shutdown).await.unwrap();
    manager.await.unwrap().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
