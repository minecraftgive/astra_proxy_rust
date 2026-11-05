use super::*;
use ratatui::{backend::TestBackend, Terminal};
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
#[tokio::test]
async fn form_builds_requests_for_each_probe_and_egress() {
    let mut view = view();
    // Type-ahead jumps to the matching outbound.
    for c in "hk".chars() {
        view.key(key(KeyCode::Char(c)));
    }
    assert_eq!(view.egresses[view.egress], Egress::Outbound("HK 01".into()));
    view.key(key(KeyCode::Right));
    assert_eq!(view.egress, 0, "←→ wraps around");
    view.key(key(KeyCode::Left));
    view.key(key(KeyCode::Left));
    assert_eq!(
        view.egresses[view.egress],
        Egress::Outbound("DIRECT".into())
    );
    let request = view.request().unwrap();
    assert_eq!(
        request.probe,
        Probe::Tcp {
            host: "www.gstatic.com".into(),
            port: 443
        }
    );
    view.key(key(KeyCode::Down));
    view.key(key(KeyCode::Char('d')));
    assert_eq!(view.kind, Kind::Dns);
    assert_eq!(view.target, "www.gstatic.com");
    assert_eq!(view.fields().len(), 6);
    view.key(key(KeyCode::Down));
    view.key(key(KeyCode::Down));
    view.key(key(KeyCode::Right));
    let request = view.request().unwrap();
    assert!(
        matches!(&request.probe, Probe::Dns { record, server, .. } if record == "AAAA" && server == "1.1.1.1")
    );
    assert_eq!(request.count, 1);
    view.field = Field::Kind;
    view.key(key(KeyCode::Char('i')));
    assert_eq!(view.fields().len(), 4);
    view.field = Field::Count;
    for c in "120".chars() {
        view.key(key(KeyCode::Char(c)));
    }
    assert!(view.count <= MAX_COUNT);
    view.field = Field::Target;
    view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(view.request().is_err());
    view.key(key(KeyCode::Enter));
    assert!(view.error.is_some() && !view.running());
    assert!(
        !view.key(key(KeyCode::Esc)),
        "Esc while idle leaves the page"
    );
}
#[tokio::test]
async fn streamed_results_render_and_stop_keeps_partial_summary() {
    let mut view = view();
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
#[tokio::test]
async fn late_outbounds_keep_the_selected_egress() {
    let mut view = view();
    view.egress = 1;
    view.set_outbounds(&["DIRECT".into(), "JP".into()]);
    assert_eq!(
        view.egresses[view.egress],
        Egress::Interface("Wi-Fi".into())
    );
    assert_eq!(view.egresses.last(), Some(&Egress::Outbound("JP".into())));
    view.egress = view.egresses.len() - 1;
    view.set_outbounds(&[]);
    assert_eq!(view.egresses[view.egress], Egress::System);
}
#[test]
fn host_part_strips_ports_but_not_ipv6() {
    assert_eq!(host_part("example.com:443"), "example.com");
    assert_eq!(host_part("[2001:db8::1]:443"), "2001:db8::1");
    assert_eq!(host_part("2001:db8::1"), "2001:db8::1");
    assert_eq!(host_part("1.1.1.1"), "1.1.1.1");
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
        view.note
            .as_deref()
            .is_some_and(|note| note.contains("outbound edge")),
        "{:?}",
        view.note
    );
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
