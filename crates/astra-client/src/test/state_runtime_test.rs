use super::*;
use astra_model::runtime::{Coverage, RuntimeTelemetry, RuntimeView, Snapshot};

fn sample(boot: &str, sequence: u64, upload: u64) -> RuntimeView {
    RuntimeView {
        snapshot: Snapshot {
            pid: 42,
            upload,
            proxy_upload: upload,
            ..Default::default()
        },
        telemetry: Some(RuntimeTelemetry {
            host_boot_id: boot.into(),
            sample_sequence: sequence,
            sample_elapsed_ms: sequence * 1000,
            coverage: Coverage::Complete,
            ..Default::default()
        }),
    }
}

#[test]
fn command_replies_do_not_replace_the_host_rate_baseline() {
    let mut client = Client::new(Kind::Proxy);
    client.accept_runtime_view("proxy".into(), sample("boot", 1, 100));
    client.accept_runtime_view("proxy".into(), sample("boot", 2, 200));
    assert_eq!(client.rates[0], 100);
    client.accept_runtime(
        "proxy".into(),
        Snapshot {
            pid: 42,
            upload: 250,
            proxy_upload: 250,
            ..Default::default()
        },
    );
    assert_eq!(
        client.runtime_telemetry.as_ref().unwrap().sample_sequence,
        2
    );
    client.accept_runtime_view("proxy".into(), sample("boot", 3, 300));
    assert_eq!(client.rates[0], 100);
    assert_eq!(client.history[0].len(), 2);
}

#[test]
fn telemetry_heartbeats_and_old_samples_do_not_rewrite_rates() {
    let mut client = Client::new(Kind::Proxy);
    client.accept_runtime_view("proxy".into(), sample("boot", 1, 100));
    client.accept_runtime_view("proxy".into(), sample("boot", 2, 200));
    client.accept_runtime_view("proxy".into(), sample("boot", 2, 200));
    client.accept_runtime_view("proxy".into(), sample("boot", 1, 100));
    assert_eq!(client.runtime_snapshot.as_ref().unwrap().upload, 200);
    assert_eq!(client.rates[0], 100);
    assert_eq!(client.history[0].len(), 1);
    let mut stale = sample("boot", 2, 200);
    stale.telemetry.as_mut().unwrap().sample_age_ms = 3001;
    client.accept_runtime_view("proxy".into(), stale);
    assert!(!client.runtime_rates_available());
}

#[test]
fn reused_pid_on_new_host_boot_resets_rates_and_history() {
    let mut client = Client::new(Kind::Proxy);
    client.accept_runtime_view("proxy".into(), sample("first", 1, 100));
    client.accept_runtime_view("proxy".into(), sample("first", 2, 200));
    client.accept_runtime_view("proxy".into(), sample("second", 1, 10));
    assert_eq!(client.rates, [0; 2]);
    assert!(client.history[0].is_empty());
    assert!(!client.runtime_rates_valid);
    client.accept_runtime_view("proxy".into(), sample("second", 2, 30));
    assert_eq!(client.rates[0], 20);
    assert!(client.runtime_rates_available());
}
