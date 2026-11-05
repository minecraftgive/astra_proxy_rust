use super::*;
use astra_model::diagnostics::Probe;
use ratatui::{backend::TestBackend, Terminal};
#[tokio::test]
async fn updates_latency_and_results_without_waiting_for_done_and_cancel_retains_results() {
    let mut view = View::start(Path::new("unused-stream.sock"), "proxy".into());
    view.task.take().unwrap().abort();
    let (sender, receiver) = mpsc::channel(16);
    view.receiver = Some(receiver);
    view.loading = false;
    view.targets = vec!["slow".into(), "fast".into()];
    view.started = Some(Instant::now());
    for event in [
        Event::Started {
            targets: view.targets.clone(),
            url: "test".into(),
            running_config: true,
        },
        Event::Running {
            outbound: "slow".into(),
        },
        Event::Running {
            outbound: "fast".into(),
        },
        Event::Connected {
            outbound: "slow".into(),
            connect_ms: 15,
        },
        Event::Finished {
            probe: Probe {
                outbound: "fast".into(),
                direct: false,
                connect_ms: Some(2),
                elapsed_ms: 3,
                ip: Some("192.0.2.1".parse().unwrap()),
                error: None,
            },
        },
    ] {
        sender.send(Ok(Update::Event(event))).await.unwrap();
    }
    view.poll();
    assert!(
        view.receiver.is_some(),
        "updates must remain visible before completion"
    );
    assert_eq!(view.tcp["slow"], 15);
    assert_eq!(view.report.as_ref().unwrap().probes.len(), 1);
    let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        screen.contains("15 ms") && screen.contains("192.0.2.1") && screen.contains("1/2"),
        "{screen}"
    );
    view.key(KeyEvent::from(KeyCode::Char('x')));
    assert!(view.receiver.is_none() && view.error.is_some());
    assert_eq!(view.report.as_ref().unwrap().probes.len(), 1);
}
#[tokio::test]
async fn selection_and_full_page_keep_long_names_on_one_row() {
    let mut view = View::start(Path::new("unused-diagnostics.sock"), "proxy".into());
    view.task.take().unwrap().abort();
    view.receiver = None;
    view.loading = false;
    let long = format!("node-{}", "龘".repeat(120));
    view.targets = vec!["DIRECT".into(), long.clone(), "third".into()];
    view.checked = view.targets.iter().cloned().collect();
    view.key(KeyEvent::from(KeyCode::Char('a')));
    assert!(view.checked.is_empty());
    view.key(KeyEvent::from(KeyCode::Down));
    view.key(KeyEvent::from(KeyCode::Char(' ')));
    assert_eq!(
        view.checked.iter().cloned().collect::<Vec<_>>(),
        vec![long.clone()]
    );
    view.report = Some(Report {
        url: "https://api.ip.sb/ip".into(),
        running_config: true,
        probes: vec![Probe {
            outbound: long,
            direct: false,
            connect_ms: Some(12),
            elapsed_ms: 100,
            ip: Some("192.0.2.1".parse().unwrap()),
            error: None,
        }],
    });
    for (width, height) in [(120, 30), (80, 18), (20, 8), (5, 3)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| view.render(f, f.area())).unwrap();
        let lines: Vec<String> = terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        if width >= 80 {
            assert_eq!(lines.iter().filter(|line| line.contains('龘')).count(), 1);
            assert!(lines.iter().any(|l| l.contains("12 ms")));
            assert!(lines.iter().any(|l| l.contains("third")));
        }
    }
    view.key(KeyEvent::from(KeyCode::Char('c')));
    view.key(KeyEvent::from(KeyCode::Enter));
    assert!(view.error.is_some());
    assert!(view.receiver.is_none());
}
