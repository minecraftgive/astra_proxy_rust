use super::*;
use crate::protocol::{self, Action};
use astra_client::input::{KeyCode, KeyEvent, KeyModifiers};
use astra_model::nettool::{Attempt, Egress, Event, Summary};
use ratatui::{backend::TestBackend, Terminal};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}
fn control(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}
fn view() -> View {
    let mut view = View::new(
        Path::new("unused-nettool.sock"),
        "proxy".into(),
        &["DIRECT".into(), "HK 01".into(), "Auto".into()],
    );
    view.egresses = vec![
        Egress::System,
        Egress::Interface("Wi-Fi".into()),
        Egress::Outbound("DIRECT".into()),
        Egress::Outbound("HK 01".into()),
    ];
    view
}
#[tokio::test]
async fn streamed_results_render_and_stop_keeps_partial_summary() {
    let mut view = view();
    view.show_advanced = true;
    let (sender, receiver) = mpsc::channel(8);
    view.receiver = Some(receiver);
    for event in [
        Event::Started {
            egress: "iface:Wi-Fi".into(),
            probe: "TCP host:443".into(),
            note: "adapter Wi-Fi (index 7, 192.0.2.10)".into(),
            running_config: true,
        },
        Event::Attempt(Attempt {
            seq: 1,
            rtt_ms: Some(12.5),
            detail: "connected to 192.0.2.1:443".into(),
            error: None,
        }),
        Event::Attempt(Attempt {
            seq: 2,
            rtt_ms: None,
            detail: String::new(),
            error: Some("timed out after 3000 ms".into()),
        }),
    ] {
        sender.send(Ok(event)).await.unwrap();
    }
    view.poll();
    assert!(view.running());
    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
    terminal.draw(|f| view.render(f, f.area())).unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        screen.contains("12.50 ms")
            && screen.contains("192.0.2.10")
            && screen.contains("timed out"),
        "{screen}"
    );
    assert!(view.key(key(KeyCode::Esc)), "Esc while running only stops");
    assert!(!view.running());
    let summary = view.summary.clone().unwrap();
    assert_eq!((summary.sent, summary.received), (2, 1));
    for (width, height) in [(40, 10), (5, 3)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| view.render(f, f.area())).unwrap();
    }
}
struct Home(PathBuf);
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[tokio::test]
#[ignore = "requires freshly built core binaries"]
async fn live_network_tools_probe_through_manager_and_profile_outbound() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A SOCKS5 outbound that accepts every CONNECT and reports the requested target.
    let socks = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = socks.local_addr().unwrap();
    let (seen, mut targets) = mpsc::channel(4);
    let stub = tokio::spawn(async move {
        while let Ok((mut stream, _)) = socks.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut greeting = [0; 2];
                stream.read_exact(&mut greeting).await?;
                stream
                    .read_exact(&mut vec![0; greeting[1] as usize])
                    .await?;
                stream.write_all(&[5, 0]).await?;
                let mut header = [0; 4];
                stream.read_exact(&mut header).await?;
                anyhow::ensure!(header == [5, 1, 0, 3], "domain CONNECT expected");
                let mut host = vec![0; stream.read_u8().await? as usize];
                stream.read_exact(&mut host).await?;
                let port = stream.read_u16().await?;
                stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                seen.send((String::from_utf8(host)?, port)).await?;
                Ok::<_, anyhow::Error>(())
            });
        }
    });
    let home = Home(std::env::temp_dir().join(format!("astra-nettool-{}", std::process::id())));
    let state = home.0.join("state");
    let socket = state.join("management.sock");
    let config = home.0.join("proxy.toml");
    std::fs::create_dir_all(&home.0).unwrap();
    std::fs::write(&config, format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='edge'\nserver='{server}'\n")).unwrap();
    let bin = crate::manager::test_binaries();
    let manager = tokio::spawn(crate::manager::serve(state, bin));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    protocol::call(
        &socket,
        Action::Add(protocol::Instance {
            name: "proxy".into(),
            kind: protocol::Kind::Proxy,
            config,
        }),
    )
    .await
    .unwrap();
    // Drive the page itself: TUI → manager → short-lived core → streamed events.
    let mut view = View::new(&socket, "proxy".into(), &["DIRECT".into(), "edge".into()]);
    view.egress = view
        .egresses
        .iter()
        .position(|egress| *egress == Egress::Outbound("edge".into()))
        .unwrap();
    view.target = "nettool.example:443".into();
    view.count = 2;
    view.key(key(KeyCode::Enter));
    tokio::time::timeout(Duration::from_secs(15), async {
        while view.running() {
            tokio::time::sleep(Duration::from_millis(20)).await;
            view.poll();
        }
    })
    .await
    .expect("the probe stream must finish");
    assert!(view.error.is_none(), "{:?}", view.error);
    assert!(
        view.probe_path
            .as_deref()
            .is_some_and(|note| note.contains("outbound:edge")),
        "{:?}",
        view.probe_path
    );
    assert!(view
        .details
        .iter()
        .any(|item| item["event"] == "context" && item["config_used"] == true));
    assert!(view.details.iter().any(|item| item["event"] == "resolution"
        && item["purpose"] == "business_target"
        && item["method"] == "remote_proxy"));
    assert!(
        view.attempts.len() == 2
            && view
                .attempts
                .iter()
                .all(|a| a.error.is_none() && a.rtt_ms.is_some()),
        "{:?}",
        view.attempts
    );
    let summary = view.summary.clone().unwrap();
    assert_eq!((summary.sent, summary.received), (2, 2));
    for _ in 0..2 {
        assert_eq!(
            targets.recv().await.unwrap(),
            ("nettool.example".to_owned(), 443)
        );
    }
    stub.abort();
    protocol::call(&socket, Action::Shutdown).await.unwrap();
    manager.await.unwrap().unwrap();
}

#[tokio::test]
async fn narrow_terminal_scroll_keeps_wide_character_tail_reachable_in_all_sections() {
    let mut view = view();
    view.show_advanced = true;
    view.note = Some(format!("{}12345Z", "中".repeat(26)));
    let mut reached_tail = false;
    for scroll in 0..8 {
        view.scroll = scroll;
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        reached_tail |= screen.contains('Z');
    }
    assert!(
        reached_tail,
        "Physical row scrolling must reach the end of a wrapped CJK result"
    );
    for section in Section::ALL {
        view.section = section;
        for (width, height) in [(20, 12), (40, 10), (5, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| view.render(frame, frame.area()))
                .unwrap();
        }
    }
}

#[test]
fn all_twelve_tools_and_new_fields_render_in_narrow_windows() {
    let mut view = view();
    for kind in Kind::ALL {
        view.section = Section::Tools;
        view.set_kind(kind);
        for field in view.fields() {
            view.field = field;
            for (width, height) in [(44, 14), (22, 10), (5, 3)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| view.render(frame, frame.area()))
                    .unwrap();
                if (width, height) == (44, 14) {
                    let screen = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(
                        screen
                            .chars()
                            .filter(|c| !c.is_whitespace())
                            .collect::<String>()
                            .contains(
                                &t(field.label())
                                    .chars()
                                    .filter(|c| !c.is_whitespace())
                                    .collect::<String>()
                            ),
                        "{kind:?}/{field:?}: {screen}"
                    );
                }
            }
        }
    }
}

#[test]
fn batch_keys_add_edit_remove_and_copy_paths_without_starting_network() {
    let mut view = view();
    view.key(control(KeyCode::Char('r')));
    assert!(view.section == Section::Findings);
    assert_eq!(view.plan.projects.len(), 5);
    assert!(!view.running());
    view.key(key(KeyCode::Char('c')));
    assert!(view.plan.comparison);
    view.key(key(KeyCode::Right));
    view.key(key(KeyCode::Char('p')));
    assert_eq!(view.plan.projects.len(), 10);
    view.key(key(KeyCode::Char('e')));
    assert!(view.section == Section::Tools);
    assert_eq!(view.plan.projects.len(), 9);
    // An unavailable remote adapter must fail safely rather than become System.
    assert!(view.add_project().is_err());
    view.key(control(KeyCode::Char('l')));
    view.key(control(KeyCode::Char('l')));
    view.key(key(KeyCode::Delete));
    assert_eq!(view.plan.projects.len(), 8);
    let mut terminal = Terminal::new(TestBackend::new(45, 15)).unwrap();
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    assert!(!view.running());
}

#[test]
fn default_results_hide_evidence_and_non_function_toggle_preserves_it() {
    let mut view = view();
    view.details.push(serde_json::json!({
        "event": "context", "config_digest": "advanced-evidence-only"
    }));
    view.details.push(serde_json::json!({
        "event": "tcp", "result": {"host": "example.test", "port": 443, "outcome": "connected"}
    }));
    view.summary = Some(Summary::from_attempts(&[Attempt {
        seq: 1,
        rtt_ms: Some(12.5),
        detail: "connected".into(),
        error: None,
    }]));
    assert!(!view.show_advanced);
    assert!(!view.output().join("\n").contains("advanced-evidence-only"));
    let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
    terminal.draw(|f| view.render(f, f.area())).unwrap();
    assert!(terminal
        .backend()
        .buffer()
        .content
        .iter()
        .any(|cell| { !cell.symbol().trim().is_empty() && cell.fg == Color::Green }));
    let before = view.section;
    for number in 1..=12 {
        view.key(key(KeyCode::F(number)));
    }
    assert!(view.section == before);
    assert!(!view.show_advanced);
    assert!(view.plan.projects.is_empty());
    view.key(control(KeyCode::Char('d')));
    assert!(view.show_advanced);
    assert!(view.output().join("\n").contains("advanced-evidence-only"));
    view.key(control(KeyCode::Char('d')));
    assert!(!view.show_advanced);
    assert_eq!(view.details.len(), 2);
}

#[test]
fn wrapped_results_keep_semantic_colors_when_scrolled() {
    let mut view = view();
    view.error = Some(format!("{}END", "错误".repeat(40)));
    let mut reached_tail = false;
    for scroll in 0..12 {
        view.scroll = scroll;
        let mut terminal = Terminal::new(TestBackend::new(24, 14)).unwrap();
        terminal.draw(|f| view.render(f, f.area())).unwrap();
        let content = &terminal.backend().buffer().content;
        for cell in content.iter().filter(|cell| cell.symbol() == "错") {
            assert_eq!(cell.fg, Color::Red);
        }
        reached_tail |= content
            .iter()
            .any(|cell| cell.symbol() == "E" && cell.fg == Color::Red);
    }
    assert!(reached_tail, "wrapped colored text must remain reachable");
}

#[test]
fn expanded_report_is_to_the_right_of_conclusions_and_keeps_its_own_wrapping() {
    let mut view = view();
    view.section = Section::Paths;
    view.error = Some("LEFT_CONCLUSION".into());
    view.details
        .push(serde_json::json!({"event":"context", "config_digest":"REPORT_ONLY_MARKER"}));
    let render = |view: &View, width, height| {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| view.render(f, f.area())).unwrap();
        terminal
    };
    let collapsed = render(&view, 140, 28);
    assert!(!collapsed
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .contains("REPORT_ONLY_MARKER"));
    view.toggle_advanced();
    let expanded = render(&view, 140, 28);
    let buffer = expanded.backend().buffer();
    let column = |start, end| {
        (4..26)
            .map(|y| {
                (start..end)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let left = column(0, 56);
    let right = column(56, 140);
    assert!(
        left.replace(' ', "").contains("结论") && left.contains("LEFT_CONCLUSION"),
        "{left}"
    );
    assert!(!left.contains("REPORT_ONLY_MARKER"));
    assert!(
        right.replace(' ', "").contains("原始报告") && right.contains("REPORT_ONLY_MARKER"),
        "{right}"
    );
    for (width, height) in [(52, 20), (20, 12), (5, 3)] {
        render(&view, width, height);
    }
}
