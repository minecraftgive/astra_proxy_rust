use super::*;
use crate::i18n::t;
use crate::protocol::{self, Action};
use astra_client::input::{KeyCode, KeyEvent};
use astra_model::nettool::{Attempt, Egress, Summary};
use ratatui::{backend::TestBackend, Terminal};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
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
fn screen(view: &View, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| view.render(f, f.area())).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .filter(|symbol| !symbol.trim().is_empty())
        .collect()
}

#[tokio::test]
async fn chain_results_and_nat_table_render_in_one_column() {
    let mut view = view();
    view.egress = 3;
    view.context.runtime = Some(json!({
        "mode": "rule", "tun": false, "system_proxy": false,
        "groups": [{"name": "HK 01", "mode": "select", "selected": "hk-node"}]
    }));
    view.attempts = vec![
        Attempt {
            seq: 1,
            rtt_ms: Some(12.5),
            detail: "connected to 192.0.2.1:443".into(),
            error: None,
        },
        Attempt {
            seq: 2,
            rtt_ms: None,
            detail: String::new(),
            error: Some("timed out after 3000 ms".into()),
        },
    ];
    view.summary = Some(Summary::from_attempts(&view.attempts));
    view.details = vec![json!({"event": "nat", "kind": "report", "report": {
        "interface_name": "en0", "physical_path": {"binding_verified": true, "route_verified": true},
        "traditional_type": "symmetric_observed", "advanced_capable": true, "unstable": false,
        "partial": false, "mapping": "address_and_port_dependent", "filtering": "address_and_port_dependent",
        "local_endpoint": "192.168.1.5:5000", "observations": [], "notes": []}})];
    let text = screen(&view, 120, 40);
    for expected in [
        "链路",
        "HK01→hk-node",
        "12.50ms",
        "timedout",
        "丢失50%",
        "▶对称型",
        "P2P",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(!view.key(key(KeyCode::Esc)), "Esc while idle leaves");
    for (width, height) in [(40, 10), (5, 3)] {
        screen(&view, width, height);
    }
}

#[test]
fn probe_path_rows_come_from_backend_topology() {
    let mut view = view();
    view.details = vec![
        json!({"event": "analysis", "findings": [], "notes": [], "topology": [{
        "path_id": "path-a", "truncated": false,
        "nodes": [{"id": "agent", "label": "Agent", "role": "agent"},
                  {"id": "t", "label": "example.com", "role": "target"}],
        "edges": [{"from": "agent", "to": "t", "section": "connection", "level": "measured",
                   "description": "", "evidence": []}],
        "attachments": []}]}),
    ];
    let text = screen(&view, 100, 30);
    assert!(text.contains("[连接]Agent─›example.com"), "{text}");
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
    view.set_kind(Kind::Tcp);
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
    assert!(view
        .details
        .iter()
        .any(|item| item["event"] == "context" && item["actual_egress"]["name"] == "edge"));
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
    for (width, height) in [(20, 12), (40, 10), (5, 3)] {
        screen(&view, width, height);
    }
}

#[test]
fn all_tools_and_fields_render_in_narrow_windows() {
    let mut view = view();
    for kind in Kind::ALL {
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
