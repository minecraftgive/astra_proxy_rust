use super::*;

fn snapshot(timestamp: u64, upload: u64) -> MonitorSnapshot {
    MonitorSnapshot {
        sampled_at_ms: timestamp.to_string(),
        instance: "proxy".into(),
        pid: Some(42),
        uptime_secs: "10".into(),
        cpu_percent: Some(12.0),
        memory_bytes: Some("4096".into()),
        virtual_memory_bytes: None,
        upload_bytes: Some(upload.to_string()),
        download_bytes: Some("0".into()),
        active_connections: Some(1),
        total_connections: Some("2".into()),
        error: None,
        internal: None,
    }
}

#[test]
fn rates_preserve_large_counters_and_reject_reset_pid_clock_and_error() {
    let old = snapshot(1000, u64::MAX - 100);
    let mut new = snapshot(2000, u64::MAX - 50);
    assert_eq!(rates(&old, &new), [Some(50.0), Some(0.0)]);
    new.upload_bytes = Some("0".into());
    assert_eq!(rates(&old, &new)[0], None);
    new.pid = Some(43);
    assert_eq!(rates(&old, &new), [None; 2]);
    new = snapshot(1000, u64::MAX - 50);
    assert_eq!(rates(&old, &new), [None; 2]);
    new.sampled_at_ms = "500".into();
    assert_eq!(rates(&old, &new), [None; 2]);
    new = snapshot(2000, u64::MAX - 50);
    new.error = Some("unavailable".into());
    assert_eq!(rates(&old, &new), [None; 2]);
}

#[test]
fn history_is_bounded_pause_resets_baseline_and_restart_clears_history() {
    let mut view = View::new("proxy".into());
    for i in 0..150 {
        view.accept(snapshot(i * 1000, i * 100));
    }
    assert_eq!(view.samples.len(), LIMIT);
    view.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(view.paused);
    assert!(view.baseline.is_none());
    view.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    view.accept(snapshot(150_000, 15_000));
    assert_eq!(view.samples.back().unwrap().rates, [None; 2]);
    let mut restarted = snapshot(151_000, 0);
    restarted.pid = Some(43);
    view.accept(restarted);
    assert_eq!(view.samples.len(), 1);
    view.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    assert!(view.samples.is_empty());
    assert!(view.baseline.is_none());
}

#[test]
fn unavailable_trend_values_are_gaps_not_zeroes() {
    assert_eq!(
        trend(&[None, Some(0.0), Some(10.0), Some(f64::NAN)], 4),
        "·▁█·"
    );
    assert_eq!(trend(&[Some(10.0)], 0), "");
}

#[test]
fn mean_duration_preserves_integer_precision_and_requires_completed_work() {
    use astra_model::monitor::{Metric, MetricKind};
    let duration = Metric {
        sampled_at_ms: None,
        name: "dns.answer.duration_us".into(),
        value: "9007199254740993".into(),
        kind: MetricKind::Counter,
        unit: "us".into(),
        description: String::new(),
    };
    let mut completed = Metric {
        sampled_at_ms: None,
        name: "dns.answer.completed".into(),
        value: "3".into(),
        kind: MetricKind::Counter,
        unit: "count".into(),
        description: String::new(),
    };
    assert_eq!(
        mean_duration_ms(&duration, &[completed.clone()]).as_deref(),
        Some("3002399751580.331 ms")
    );
    completed.value = "0".into();
    assert!(mean_duration_ms(&duration, &[completed.clone()]).is_none());
    completed.value = "invalid".into();
    assert!(mean_duration_ms(&duration, &[completed]).is_none());
    assert!(mean_duration_ms(&duration, &[]).is_none());
}

#[tokio::test]
async fn in_flight_requests_do_not_overlap_and_pause_or_drop_cancels_them() {
    let mut view = View::new("proxy".into());
    let pending = tokio::spawn(std::future::pending::<Result<MonitorSnapshot>>());
    let id = pending.id();
    let abort = pending.abort_handle();
    view.pending = Some(pending);
    assert!(!view.poll(std::path::Path::new("unused")).await);
    assert_eq!(view.pending.as_ref().unwrap().id(), id);
    view.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
    assert!(!view.poll(std::path::Path::new("unused")).await);
    assert!(view.pending.is_none());
    let pending = tokio::spawn(std::future::pending::<Result<MonitorSnapshot>>());
    let abort = pending.abort_handle();
    view.pending = Some(pending);
    drop(view);
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
}

#[test]
fn monitor_renders_in_all_languages_and_small_terminals() {
    use crate::i18n::{with_language, Language};
    let mut view = View::new("proxy".into());
    let mut sample = snapshot(1000, 100);
    sample.internal = Some(astra_model::monitor::InternalSnapshot {
        epoch: "core-epoch".into(),
        metrics: vec![astra_model::monitor::Metric {
            sampled_at_ms: None,
            name: "dns.cache.hits".into(),
            value: "9007199254740993".into(),
            kind: astra_model::monitor::MetricKind::Counter,
            unit: "count".into(),
            description: "DNS 缓存命中".into(),
        }],
    });
    view.accept(sample);
    for language in [
        Language::English,
        Language::Simplified,
        Language::Traditional,
    ] {
        with_language(language, || {
            for (width, height) in [(120, 24), (60, 20), (20, 8), (8, 4)] {
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| view.render(f, f.area())).unwrap();
                if width == 120 {
                    let content = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(content.contains("dns.cache.hits"));
                    assert!(content.contains("9007199254740993"));
                }
            }
        });
    }
}

#[test]
fn shortcut_captures_input_and_preserves_the_underlying_draft() {
    let mut ui = Ui::new(Session::default());
    ui.page = PROFILES;
    ui.rows.push(Row {
        instance: Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: "proxy.toml".into(),
        },
        pid: Some(42),
        state: "running".into(),
        uptime: 10,
        changed: false,
    });
    ui.editor = Some(Editor::new("proxy".into(), "standalone=true\n".into()));
    ui.prompt(Prompt::AddName);
    ui.paste_prompt("pending name");
    let editor = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
    ui.key(event::KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
    assert_eq!(ui.monitor.as_ref().unwrap().name, "proxy");
    ui.paste("must not reach the draft");
    ui.key(event::KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    ));
    ui.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert!(ui.monitor.as_ref().unwrap().paused);
    ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(ui.monitor.is_none());
    assert_eq!(ui.input, "pending name");
    assert_eq!(
        serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
        editor
    );
    ui.key(event::KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE));
    ui.key(event::KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
    assert!(ui.monitor.is_none());
    assert!(ui.palette.is_some());
}
