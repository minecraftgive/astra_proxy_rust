use super::*;
use crate::protocol::Instance;

fn row(name: &str, kind: Kind, pid: Option<u32>) -> Row {
    Row {
        instance: Instance {
            name: name.into(),
            kind,
            config: format!("{name}.toml").into(),
        },
        pid,
        state: if pid.is_some() { "Running" } else { "Stopped" }.into(),
        uptime: 0,
        changed: false,
    }
}

fn part(module: Module, name: &str, bytes: Option<u64>) -> Part {
    Part {
        module,
        name: name.into(),
        pid: Some(1),
        bytes,
        error: bytes.is_none().then(|| "unreadable".into()),
    }
}

#[test]
fn targets_cover_running_cores_by_module() {
    let rows = [
        row("rules", Kind::Rule, Some(30)),
        row("b-proxy", Kind::Proxy, Some(20)),
        row("stopped", Kind::Proxy, None),
        row("a-proxy", Kind::Proxy, Some(10)),
    ];
    assert_eq!(
        targets(&rows),
        vec![
            (Module::Proxy, "a-proxy".into(), 10),
            (Module::Proxy, "b-proxy".into(), 20),
            (Module::Rule, "rules".into(), 30),
        ]
    );
}

#[test]
fn composition_sums_modules_and_shares_of_readable_total() {
    let composition = Composition {
        parts: vec![
            part(Module::Interface, "astra-gui", Some(100)),
            part(Module::Proxy, "a", Some(200)),
            part(Module::Proxy, "b", Some(100)),
            part(Module::Proxy, "c", None),
            part(Module::Rule, "rules", Some(600)),
        ],
    };
    assert_eq!(composition.total(), 1000);
    assert_eq!(composition.module_bytes(Module::Proxy), Some(300));
    assert_eq!(composition.module_bytes(Module::Server), None);
    let summaries = composition.summaries();
    // Server only appears when such a core runs.
    assert_eq!(
        summaries.iter().map(|s| s.module).collect::<Vec<_>>(),
        vec![Module::Interface, Module::Proxy, Module::Rule]
    );
    assert_eq!(summaries[1].parts.len(), 3);
    assert!((summaries[1].share - 0.3).abs() < 1e-9);
    assert!((summaries.iter().map(|s| s.share).sum::<f64>() - 1.0).abs() < 1e-9);
}

#[test]
fn absent_cores_stay_listed_without_a_share() {
    let composition = Composition {
        parts: vec![part(Module::Interface, "astra-gui", Some(64))],
    };
    let summaries = composition.summaries();
    assert_eq!(summaries.len(), 3);
    assert_eq!(summaries[2].module, Module::Rule);
    assert_eq!(summaries[2].bytes, None);
    assert_eq!(summaries[2].share, 0.0);
    assert_eq!(Composition::default().share(10), 0.0);
}

#[test]
fn sampling_runs_only_while_a_watcher_is_visible() {
    let mut view = View::new(Default::default());
    assert!(!view.active());
    view.watch(1, false);
    assert!(!view.active());
    view.watch(2, true);
    assert!(view.active());
    view.set_paused(true);
    assert!(!view.active());
    view.set_paused(false);
    view.watch(2, false);
    assert!(!view.active());
    assert!(!view.unwatch(2));
    assert!(view.unwatch(1));
}

#[tokio::test]
async fn hidden_view_never_starts_a_round() {
    let mut view = View::new(Default::default());
    view.watch(1, false);
    let rows = [row("p", Kind::Proxy, Some(1))];
    assert!(!view.poll_ready(Path::new(""), &rows));
    assert!(view.pending.is_none());
    view.watch(1, true);
    view.poll_ready(Path::new(""), &rows);
    assert!(view.pending.is_some());
    // Hiding cancels the round in flight.
    view.watch(1, false);
    assert!(view.pending.is_none());
}

#[tokio::test]
async fn a_round_records_this_process_and_history() {
    let mut view = View::new(Default::default());
    view.watch(1, true);
    // No manager socket: the round holds this interface process alone.
    view.poll_ready(Path::new(""), &[]);
    let pending = view.pending.as_ref().unwrap();
    while !pending.is_finished() {
        tokio::task::yield_now().await;
    }
    assert!(view.poll_ready(Path::new(""), &[]));
    let latest = view.latest.as_ref().unwrap();
    assert_eq!(latest.parts.len(), 1);
    assert_eq!(latest.parts[0].module, Module::Interface);
    assert_eq!(latest.parts[0].pid, Some(std::process::id()));
    assert!(latest.parts[0].bytes.unwrap_or(0) > 0);
    assert_eq!(view.history.len(), 1);
    assert!(view.series(Module::Interface)[0].is_some());
    assert_eq!(view.series(Module::Proxy), vec![None]);
    view.clear();
    assert!(view.latest.is_none() && view.history.is_empty());
}

#[test]
fn history_is_bounded() {
    let mut view = View::new(Default::default());
    for _ in 0..LIMIT + 5 {
        view.accept(vec![part(Module::Rule, "rules", Some(1))]);
    }
    assert_eq!(view.history.len(), LIMIT);
}
