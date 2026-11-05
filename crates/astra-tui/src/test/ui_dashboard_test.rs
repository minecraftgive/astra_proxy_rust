use super::super::shell::{OVERVIEW, PROFILES};
use super::*;
use ratatui::backend::TestBackend;

#[test]
fn escape_returns_through_page_history_after_closing_dialogs() {
    let mut ui = Ui::new(Session::default());
    ui.navigate(PROFILES);
    ui.navigate(PROFILES); // Reselecting a page must not add a frame.
    ui.navigate(PROXIES);
    ui.help_scroll = Some(0);
    let escape = event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    ui.key(escape);
    assert!(ui.help_scroll.is_none());
    assert_eq!(ui.page, PROXIES);
    ui.key(escape);
    assert_eq!(ui.page, PROFILES);
    ui.key(escape);
    assert_eq!(ui.page, OVERVIEW);
    ui.key(escape);
    assert_eq!(ui.page, OVERVIEW);
    assert!(ui.navigation_stack.is_empty());
}

fn sample() -> Ui {
    let mut ui = Ui::new(Session::default());
    ui.rows = (0..24)
        .map(|index| Row {
            instance: Instance {
                name: format!("proxy-{index:02}"),
                kind: Kind::Proxy,
                config: format!("/profiles/proxy-{index:02}.toml").into(),
            },
            pid: Some(100 + index),
            state: "Running".into(),
            uptime: 3723,
            changed: false,
        })
        .collect();
    ui.details.insert(
        "proxy-00".into(),
        InstanceDetails {
            modes: vec!["Rules".into(), "System proxy".into()],
            listeners: vec!["SOCKS5 127.0.0.1:7890".into()],
            outbounds: vec!["Tokyo [select]".into(), "Singapore [url-test]".into()],
            outbound_count: 12,
            rules: 114,
            diagnostics_supported: true,
            ..InstanceDetails::default()
        },
    );
    ui
}
fn click(rect: Rect) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x,
        row: rect.y,
        modifiers: KeyModifiers::NONE,
    }
}
fn render(ui: &Ui, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| ui.render(f)).unwrap();
    terminal
}
fn screen(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
fn live_sample() -> Ui {
    use astra_model::runtime::Connection;
    use astra_model::runtime::Group;
    use astra_model::runtime::Member;
    use astra_model::runtime::Snapshot;
    let mut ui = sample();
    ui.accept_runtime(
        "proxy-00".into(),
        Snapshot {
            pid: 100,
            mode: Mode::Rule,
            system_proxy: true,
            active: 3,
            total: 9,
            upload: 50_000,
            download: 900_000,
            // Deliberately unsorted: the Overview orders by total traffic.
            connections: vec![
                Connection {
                    id: 3,
                    source: None,
                    host: "data.bilibili.com".into(),
                    port: 443,
                    network: "Udp".into(),
                    inbound: "Tun".into(),
                    outbound: "DIRECT".into(),
                    upload: 353_500,
                    download: 87_600,
                    elapsed_ms: 3000,
                },
                Connection {
                    id: 2,
                    source: None,
                    host: "claude.ai".into(),
                    port: 443,
                    network: "Tcp".into(),
                    inbound: "Mixed".into(),
                    outbound: "AJP-80-Direct-Vision".into(),
                    upload: 1_400_000,
                    download: 44_900,
                    elapsed_ms: 3000,
                },
                Connection {
                    id: 1,
                    source: None,
                    host: "j-cn-live-comet.chat.bilibili.com".into(),
                    port: 2245,
                    network: "Tcp".into(),
                    inbound: "Mixed".into(),
                    outbound: "DIRECT".into(),
                    upload: 18_800,
                    download: 19_800_000,
                    elapsed_ms: 3000,
                },
            ],
            groups: vec![
                Group {
                    name: "PROXY".into(),
                    mode: "select".into(),
                    selected: Some("AJP-80-Direct-Vision".into()),
                    testing: false,
                    members: vec![Member {
                        name: "AJP-80-Direct-Vision".into(),
                        delay_ms: Some(38),
                        alive: Some(true),
                    }],
                },
                Group {
                    name: "DIRECT".into(),
                    mode: "select".into(),
                    selected: Some("DIRECT".into()),
                    testing: false,
                    members: vec![],
                },
            ],
            ..Snapshot::default()
        },
    );
    ui.accept_runtime("proxy-00".into(), ui.live().unwrap().clone());
    ui.rates = [2_300, 762_900];
    for value in 0..64u64 {
        ui.history[0].push_back(if value % 3 == 0 { 2_300 } else { 0 });
        ui.history[1].push_back(762_900 - value * 8_000);
    }
    ui
}
#[test]
fn retained_pid_does_not_make_an_unresponsive_core_look_healthy() {
    let mut ui = sample();
    ui.rows[0].state = "Unresponsive".into();
    assert_eq!(indicator_state(&ui.rows[0]), ("●R", Color::Red));
    ui.rows[0].state = "Starting".into();
    assert_eq!(indicator_state(&ui.rows[0]), ("●Y", Color::Yellow));
}
#[test]
fn network_recovery_notice_keeps_conflict_evidence_visible() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = sample();
        ui.details.get_mut("proxy-00").unwrap().network_recovery =
            Some(astra_model::recovery::Notice {
                state: astra_model::recovery::RecoveryState::Conflict,
                timestamp_ms: 1700000000000,
                message: "Route changed externally; retained".into(),
                journal: "C:/Astra/recovery.json".into(),
                boot_id: "boot".into(),
            });
        let notices = ui.notices();
        assert_eq!(notices.len(), 3);
        assert!(notices.iter().all(|(color, _)| *color == Color::Red));
        assert!(
            notices[0].1.contains("Network recovery conflict") && notices[0].1.contains("2023-")
        );
        assert!(notices[1].1.contains("retained"));
        assert!(notices[2].1.contains("C:/Astra/recovery.json"));
        assert!(ui.next_notice_expiry().is_none());
        let old = Instant::now().checked_sub(Duration::from_secs(16)).unwrap();
        for (_, message) in &notices {
            ui.notice_since.borrow_mut().insert(message.clone(), old);
        }
        assert_eq!(ui.notices(), notices, "conflicts never time out");
        ui.details
            .get_mut("proxy-00")
            .unwrap()
            .network_recovery
            .as_mut()
            .unwrap()
            .state = astra_model::recovery::RecoveryState::Pending;
        let pending = ui.notices();
        assert_eq!(pending.len(), 3);
        assert!(pending.iter().all(|(color, _)| *color == Color::Red));
        assert!(ui.next_notice_expiry().is_none());
    });
}
#[test]
fn network_recovery_success_expires_without_reviving_on_redraw() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = sample();
        ui.details.get_mut("proxy-00").unwrap().network_recovery =
            Some(astra_model::recovery::Notice {
                state: astra_model::recovery::RecoveryState::Recovered,
                timestamp_ms: 1700000000000,
                message: "Owned network settings restored".into(),
                journal: "C:/Astra/recovery.json".into(),
                boot_id: "boot".into(),
            });
        let text = screen(&render(&ui, 140, 40));
        assert!(text.contains("Attention"));
        assert!(text.contains("Network settings recovered"));
        assert!(text.contains("Owned network settings restored"));
        assert!(text.contains("C:/Astra/recovery.json"));
        assert_eq!(ui.notices().len(), 3);
        assert!(ui.next_notice_expiry().is_some());
        assert!(!ui.expire_notices(), "not due yet");

        let old = Instant::now().checked_sub(Duration::from_secs(16)).unwrap();
        for shown in ui.notice_since.borrow_mut().values_mut() {
            *shown = old;
        }
        ui.notice_expiry.set(Some(old + Duration::from_secs(15)));
        assert!(ui.expire_notices(), "wake even without input or telemetry");
        assert!(ui.next_notice_expiry().is_none());
        assert!(!ui.expire_notices(), "consume the idle wakeup only once");
        for _ in 0..2 {
            let text = screen(&render(&ui, 140, 40));
            assert!(!text.contains("Attention"));
            assert!(!text.contains("Network settings recovered"));
            assert!(!text.contains("Owned network settings restored"));
            assert!(!text.contains("C:/Astra/recovery.json"));
            assert!(ui.next_notice_expiry().is_none());
        }
        assert!(ui.details["proxy-00"].network_recovery.is_some());
    });
}
#[test]
fn overview_notice_expiry_waits_until_overview_is_visible() {
    let mut ui = sample();
    ui.rows[0].changed = true;
    assert_eq!(ui.notices().len(), 1);
    let deadline = ui.next_notice_expiry().unwrap();
    ui.page = PROFILES;
    assert!(ui.next_notice_expiry().is_none());
    assert!(!ui.expire_notices());
    ui.page = OVERVIEW;
    assert_eq!(ui.next_notice_expiry(), Some(deadline));
    ui.rows[0].changed = false;
    assert!(ui.notices().is_empty());
    assert!(ui.next_notice_expiry().is_none());
}
#[test]
fn overview_attention_hides_warnings_but_keeps_errors() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = sample();
        ui.rows[0].state = "Exited(1)".into();
        ui.rows[0].pid = None;
        ui.rows[0].changed = true;
        assert_eq!(ui.notices().len(), 2, "error and warning both shown");
        let old = Instant::now().checked_sub(Duration::from_secs(16)).unwrap();
        for shown in ui.notice_since.borrow_mut().values_mut() {
            *shown = old;
        }
        let notices = ui.notices();
        assert_eq!(notices.len(), 1, "warning timed out");
        assert_eq!(notices[0].0, Color::Red, "error persists");
        assert!(notices[0].1.contains("Core stopped unexpectedly"));
        assert!(ui.next_notice_expiry().is_none());
    });
}
#[test]
fn overview_layout_prioritizes_connections_and_adapts_to_width() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let ui = live_sample();
        let text = screen(&render(&ui, 140, 40));
        assert_eq!(text.matches("Astra").count(), 1);
        assert!(text.contains("├─ 1 Overview"));
        assert!(text.contains("LIVE CONNECTIONS · 3/3"));
        assert!(text.contains("SNAPSHOT"));
        assert!(text.contains("●G proxy-00 · P"));
        assert!(text.contains("CONTROLS"));
        assert!(text.contains("S System proxy ●"));
        assert!(text.contains("N Network tools"));
        assert!(
            "▁▂▃▄▅▆▇█".chars().any(|glyph| text.contains(glyph)),
            "embedded traffic chart missing"
        );
        let order = ["j-cn-live-comet", "claude.ai", "data.bilibili.com"]
            .map(|host| text.find(host).unwrap());
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "connections are not sorted by traffic: {order:?}"
        );

        let text = screen(&render(&ui, 100, 30));
        assert!(text.contains("PROXY GROUPS · 2"));
        assert!(!text.contains("SNAPSHOT"));
        assert!(text.contains("m Rule"));
        assert!(text.contains("S System proxy ●"));

        let text = screen(&render(&ui, 70, 30));
        assert!(text.contains("Astra · proxy-00 · ● Connected"));
        assert_eq!(text.matches("Connected").count(), 1);
        assert!(text.contains("├─ 1 Overview"));
        assert!(text.contains("DIRECT · Tcp"));
        assert!(text.contains("S proxy ●"));
        assert!(text.contains("A LAN ○"));
        assert!(!text.contains("SNAPSHOT"));
    });
}
#[test]
fn header_rates_keep_columns_and_actions_stay_in_controls() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        for width in [70, 100, 140] {
            let mut ui = live_sample();
            let mut columns = None;
            for value in [0, 1_023, 1_048_575, 1 << 30, u64::MAX] {
                ui.rates = [value, value / 2];
                ui.direct_rates = [value / 3, value / 4];
                let terminal = render(&ui, width, 30);
                let buffer = terminal.backend().buffer();
                let up: Vec<_> = (0..width)
                    .filter(|&x| buffer[(x, 1)].symbol() == "↑")
                    .collect();
                let down: Vec<_> = (0..width)
                    .filter(|&x| buffer[(x, 2)].symbol() == "↓")
                    .collect();
                assert_eq!(up.len(), 2, "both rate columns at width {width}");
                assert_eq!(up, down, "upload and download columns align");
                assert_eq!(columns.get_or_insert(up.clone()), &up);
                let footer: String = (0..width).map(|x| buffer[(x, 28)].symbol()).collect();
                assert!(footer.contains("F2") && footer.contains("F1"));

                let controls_y = (0..30)
                    .find(|&y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                            .contains("CONTROLS")
                    })
                    .unwrap();
                let hits = ui.hits.borrow();
                let tools = hits
                    .iter()
                    .find(|(_, target)| matches!(target, Target::NetTools))
                    .unwrap()
                    .0;
                let connect = hits
                    .iter()
                    .find(|(_, target)| matches!(target, Target::Connect))
                    .unwrap()
                    .0;
                assert!(tools.y > controls_y);
                assert!(tools.y < controls_y + ui.runtime_controls_height(width) - 1);
                assert_eq!(tools.y, connect.y);
                assert!(tools.right() < connect.x);
                assert!(connect.right() >= width - 3 && connect.right() < width);
            }
            // Missing telemetry keeps the same columns as live values.
            let terminal = render(&sample(), width, 30);
            let buffer = terminal.backend().buffer();
            let up: Vec<_> = (0..width)
                .filter(|&x| buffer[(x, 1)].symbol() == "↑")
                .collect();
            assert_eq!(columns.unwrap(), up);
        }
    });
}

#[test]
fn overview_expands_traffic_and_stays_stable_in_small_windows() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = live_sample();
        ui.key(event::KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        let text = screen(&render(&ui, 140, 40));
        assert!(text.contains("Proxy traffic · z collapse"));
        assert!(text.contains("z collapse"));
        ui.key(event::KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(!screen(&render(&ui, 140, 40)).contains("Traffic"));
        for (width, height) in [(1, 1), (8, 4), (20, 6), (60, 20), (90, 24)] {
            let _ = render(&ui, width, height);
        }
    });
}
#[test]
fn narrow_overview_scrolls_from_connections_into_proxy_groups() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        use astra_model::runtime::Group;
        use astra_model::runtime::Member;
        let mut ui = live_sample();
        let mut snapshot = ui.live().unwrap().clone();
        for index in 0..10 {
            snapshot.groups.push(Group {
                name: format!("GROUP-{index}"),
                mode: "select".into(),
                selected: Some(format!("node-{index}")),
                testing: false,
                members: vec![Member {
                    name: format!("node-{index}"),
                    delay_ms: Some(20 + index),
                    alive: Some(true),
                }],
            });
        }
        ui.accept_runtime("proxy-00".into(), snapshot);
        let _ = render(&ui, 70, 30);
        let limit = ui.overview_item_count.get();
        assert_eq!(limit, 15);
        for _ in 0..limit {
            ui.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let text = screen(&render(&ui, 70, 30));
        assert!(
            text.contains("GROUP-9"),
            "proxy groups did not scroll: {text}"
        );
    });
}
#[test]
fn overview_folds_shared_targets_and_renders_aggregate_traffic() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        for width in [140, 70] {
            let mut ui = live_sample();
            let mut snapshot = ui.live().unwrap().clone();
            let mut first = snapshot.connections[0].clone();
            first.id = 10;
            first.host = "chatgpt.com".into();
            first.port = 443;
            first.upload = 1024;
            first.download = 2048;
            let mut second = first.clone();
            second.id = 11;
            second.upload = 2048;
            second.download = 4096;
            snapshot.connections = vec![first, second];
            snapshot.active = 2;
            ui.accept_runtime("proxy-00".into(), snapshot);
            let text = screen(&render(&ui, width, 30));
            assert!(text.contains("▸ chatgpt.com:443 (2)"), "{text}");
            assert!(text.contains("3.0 KiB") && text.contains("6.0 KiB"));
            assert!(!ui
                .hits
                .borrow()
                .iter()
                .any(|(_, target)| matches!(target, Target::Connection(_))));
            let header = ui
                .hits
                .borrow()
                .iter()
                .find_map(|(area, target)| {
                    matches!(target, Target::ConnectionRow(_)).then_some(*area)
                })
                .unwrap();
            ui.mouse(click(header));
            assert_eq!(ui.page, OVERVIEW);
            let text = screen(&render(&ui, width, 30));
            assert!(text.contains("▾ chatgpt.com:443 (2)"), "{text}");
            for id in [10, 11] {
                assert!(ui.hits.borrow().iter().any(
                    |(_, target)| matches!(target, Target::Connection(value) if *value == id)
                ));
            }
            let header = ui
                .hits
                .borrow()
                .iter()
                .find_map(|(area, target)| {
                    matches!(target, Target::ConnectionRow(_)).then_some(*area)
                })
                .unwrap();
            ui.mouse(click(header));
            render(&ui, width, 30);
            assert!(!ui
                .hits
                .borrow()
                .iter()
                .any(|(_, target)| matches!(target, Target::Connection(_))));
        }
    });
}
#[test]
fn mouse_navigation_actions_scrolling_and_modal_capture() {
    let mut ui = sample();
    render(&ui, 120, 36);
    let rect = ui
        .hits
        .borrow()
        .iter()
        .find(|(_, hit)| matches!(hit, Target::Page(PROFILES)))
        .unwrap()
        .0;
    ui.mouse(click(rect));
    assert_eq!(ui.page, PROFILES);
    render(&ui, 120, 36);
    let rect = ui
        .hits
        .borrow()
        .iter()
        .find(|(_, hit)| matches!(hit, Target::Row(1)))
        .unwrap()
        .0;
    ui.mouse(click(rect));
    assert_eq!(ui.selections[0], 1);
    assert_eq!(ui.menu, Some(0));
    render(&ui, 120, 36);
    assert!(ui
        .hits
        .borrow()
        .iter()
        .all(|(_, hit)| matches!(hit, Target::Menu(_))));
    let rect = ui.hits.borrow()[0].0;
    assert!(
        matches!(ui.mouse(click(rect)), Input::Run(Action::Stop(name), _) if name == "proxy-01")
    );
    ui.menu = None;
    for _ in 0..20 {
        ui.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 15,
            modifiers: KeyModifiers::NONE,
        });
    }
    assert_eq!(ui.selections[0], 21);
    render(&ui, 120, 36);
    assert!(ui
        .hits
        .borrow()
        .iter()
        .any(|(_, hit)| matches!(hit, Target::Row(21))));
    ui.prompt(Prompt::Shutdown);
    let before = ui.page;
    ui.mouse(click(Rect::new(1, 1, 1, 1)));
    assert_eq!(ui.page, before);
    assert!(ui.prompt.is_some());
}
#[test]
fn overview_resizes_follows_active_profile_and_palette_runs_commands() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut ui = sample();
        let text = screen(&render(&ui, 140, 40));
        for value in [
            "1 Overview",
            "6 Profiles",
            "Connected",
            "proxy-00",
            "SOCKS5 127.0.0.1:7890",
            "1h02m",
        ] {
            assert!(text.contains(value), "{value}");
        }
        for (width, height) in [(1, 1), (20, 6), (60, 20), (90, 24)] {
            render(&ui, width, height);
        }
        // c toggles the active profile from any page except Profiles.
        assert!(matches!(
            ui.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Input::Run(Action::Stop(name), _) if name == "proxy-00"
        ));
        ui.key(event::KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        for c in "node".chars() {
            ui.key(event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        render(&ui, 100, 30);
        assert!(matches!(
            ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Input::Run(Action::Diagnostics(name), _) if name == "proxy-00"
        ));
        assert!(ui.palette.is_none());
        // p lists profiles; choosing one makes it active everywhere.
        ui.key(event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        ui.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        ui.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(ui.active_proxy().unwrap().instance.name, "proxy-02");
        assert_eq!(ui.page, OVERVIEW);
        if std::env::var_os("ASTRA_TUI_PREVIEW").is_some() {
            ui.page = OVERVIEW;
            let terminal = render(&ui, 140, 40);
            let cells: Vec<_> = terminal.backend().buffer().content.iter().map(|cell| serde_json::json!({"text":cell.symbol(),"fg":format!("{:?}",cell.fg),"bg":format!("{:?}",cell.bg)})).collect();
            std::fs::write(
                concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tui-preview.json"),
                serde_json::to_vec(&cells).unwrap(),
            )
            .unwrap();
        }
    });
}
#[tokio::test]
async fn overview_opens_network_tools_and_esc_returns_to_overview() {
    let mut ui = sample();
    let press = |ui: &mut Ui, code| ui.key(event::KeyEvent::new(code, KeyModifiers::NONE));
    let Input::Run(action, _) = press(&mut ui, KeyCode::Char('N')) else {
        panic!("N should open network tools")
    };
    assert!(matches!(&action, Action::Diagnostics(name) if name == "proxy-00"));
    ui.execute(std::path::Path::new("unused-overview.sock"), action, None)
        .await;
    let view = ui.diagnostics.as_ref().expect("network tools open");
    assert!(view.handles_escape(), "opens straight into the tools page");
    assert!(!ui.open_nettools, "the request flag is consumed");
    let text = crate::i18n::with_language(crate::i18n::Language::English, || {
        screen(&render(&ui, 120, 30))
    });
    assert!(
        text.contains("Network tools") && text.contains("Egress"),
        "{text}"
    );
    press(&mut ui, KeyCode::Esc);
    // The Overview card offers the same entry as a button.
    render(&ui, 140, 40);
    let button = ui
        .hits
        .borrow()
        .iter()
        .find(|(_, hit)| matches!(hit, Target::NetTools))
        .map(|(rect, _)| *rect)
        .expect("network tools button on Overview");
    let click = ui.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: button.x,
        row: button.y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(matches!(click, Input::Run(Action::Diagnostics(_), _)) && ui.open_nettools);
    ui.open_nettools = false;
    assert!(ui.diagnostics.is_none());
    assert_eq!(ui.page, OVERVIEW);
    for live in [false, true] {
        let mut controls = if live { live_sample() } else { sample() };
        controls.focus.region = focus::Region::Controls;
        controls.focus.cursor = if live { 6 } else { 0 };
        assert!(matches!(
            press(&mut controls, KeyCode::Enter),
            Input::Run(Action::Diagnostics(_), _)
        ));
        assert!(controls.open_nettools);
        press(&mut controls, KeyCode::Right);
        assert!(matches!(
            press(&mut controls, KeyCode::Enter),
            Input::Run(Action::Stop(_), _)
        ));
    }
    // D still opens the node list, where n reaches the same tools.
    let Input::Run(action, _) = press(&mut ui, KeyCode::Char('D')) else {
        panic!()
    };
    ui.execute(std::path::Path::new("unused-overview.sock"), action, None)
        .await;
    assert!(!ui.diagnostics.as_ref().unwrap().handles_escape());
}
