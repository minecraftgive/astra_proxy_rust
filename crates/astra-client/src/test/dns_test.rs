use super::*;
use crate::protocol::{Instance, Kind, Row};
use astra_model::runtime::{Coverage, DnsResolverStatus, RuntimeTelemetry, RuntimeView, Snapshot};
use std::{path::Path, time::Duration};

fn client() -> Client {
    let mut client = Client::new(Kind::Proxy);
    client.rows = vec![Row {
        instance: Instance {
            name: "dns-profile".into(),
            kind: Kind::Proxy,
            config: "dns.toml".into(),
        },
        pid: Some(123),
        state: "Running".into(),
        uptime: 1,
        changed: false,
    }];
    client.accept_runtime_view(
        "dns-profile".into(),
        RuntimeView {
            snapshot: Snapshot {
                pid: 123,
                ..Default::default()
            },
            telemetry: Some(RuntimeTelemetry {
                host_boot_id: "boot".into(),
                active_generation: Some(5),
                coverage: Coverage::Complete,
                dns_resolver: Some(Box::new(DnsResolverStatus {
                    enabled: true,
                    listen: vec!["0.0.0.0:1053".parse().unwrap()],
                    ..Default::default()
                })),
                ..Default::default()
            }),
        },
    );
    client.dns_state.target = "example.com.".into();
    client
}

#[test]
fn query_uses_observed_listener_and_rejects_missing_or_stale_settings() {
    crate::i18n::with_language(crate::i18n::Language::English, || {
        let mut client = client();
        client.dns_state.record = 1;
        let request = client.prepare_dns_query().unwrap();
        assert_eq!(request.server, "tcp://127.0.0.1:1053");
        assert_eq!(request.record, "AAAA");
        for target in [
            "",
            "https://example.com",
            "example..com",
            "example.com..",
            "bad name",
        ] {
            client.dns_state.target = target.into();
            assert!(client.prepare_dns_query().is_err(), "{target}");
        }
        client.dns_state.target = "_dns.example.com".into();
        client
            .runtime_telemetry
            .as_mut()
            .unwrap()
            .dns_resolver
            .as_mut()
            .unwrap()
            .listen = vec!["[::]:1053".parse().unwrap()];
        assert_eq!(
            client.prepare_dns_query().unwrap().server,
            "tcp://[::1]:1053"
        );
        client
            .runtime_telemetry
            .as_mut()
            .unwrap()
            .dns_resolver
            .as_mut()
            .unwrap()
            .listen
            .clear();
        assert!(client
            .prepare_dns_query()
            .err()
            .unwrap()
            .contains("No DNS listener"));
        client
            .runtime_telemetry
            .as_mut()
            .unwrap()
            .dns_resolver
            .as_mut()
            .unwrap()
            .enabled = false;
        assert!(client
            .prepare_dns_query()
            .err()
            .unwrap()
            .contains("disabled"));
        client.runtime_telemetry.as_mut().unwrap().dns_resolver = None;
        assert!(client
            .prepare_dns_query()
            .err()
            .unwrap()
            .contains("unavailable"));
        client.runtime_telemetry.as_mut().unwrap().sample_age_ms = u64::MAX;
        assert!(client
            .prepare_dns_query()
            .err()
            .unwrap()
            .contains("fresh runtime"));
    });
}

#[test]
fn changed_profile_generation_and_cancel_never_dispatch_a_pending_query() {
    let mut client = client();
    client.request_dns_query();
    assert!(client.dns_query_running());
    client.runtime_telemetry.as_mut().unwrap().active_generation = Some(6);
    assert!(client.poll_dns_query(Path::new("unused.sock")));
    assert!(!client.dns_query_running());
    assert!(client.dns_state.query.is_none());
    assert!(client.dns_state.query_error.is_some());
    client.request_dns_query();
    client.cancel_dns_query();
    assert!(!client.poll_dns_query(Path::new("unused.sock")));
    client.request_dns_query();
    client.reset_runtime_identity();
    assert!(!client.dns_query_running());
    assert!(client.dns_state.query.is_none());
}

#[tokio::test]
async fn query_dispatches_once_to_agent_and_retains_dns_answer() {
    use astra_management::v2 as v;
    use astra_model::nettool::{Attempt, Egress, Event, Probe, Summary};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("dns-query.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let executions = Arc::new(AtomicUsize::new(0));
    let observed = executions.clone();
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => accepted.unwrap(),
            };
            let request: v::Request = astra_management::client::receive(&mut stream)
                .await
                .unwrap();
            let body = match request.body {
                v::RequestBody::Query(v::Query::Hello) => v::ResponseBody::Hello(v::Hello {
                    version: v::VERSION,
                    role: "agent".into(),
                    epoch: "boot".into(),
                    build_id: "fixture".into(),
                    capabilities: vec![
                        v::NETWORK_DIAGNOSTICS_CAPABILITY.into(),
                        "network_diagnostics_tools_v1".into(),
                        "network_diagnostics_resolution_v1".into(),
                    ],
                }),
                v::RequestBody::Query(v::Query::Runtime { .. }) => {
                    v::ResponseBody::Runtime(v::RuntimeSnapshot::Client(serde_json::json!({})))
                }
                v::RequestBody::Execute {
                    operation_id,
                    command: v::Command::NetworkDiagnostics { instance, request },
                } => {
                    observed.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(instance, "dns-profile");
                    assert_eq!(request.egress, Egress::System);
                    assert_eq!(request.count, 1);
                    assert_eq!(
                        request.probe,
                        Probe::Dns {
                            name: "example.com.".into(),
                            record: "A".into(),
                            server: "tcp://127.0.0.1:1053".into()
                        }
                    );
                    v::ResponseBody::Task(v::Task {
                        id: operation_id,
                        state: v::TaskState::Running,
                        message: "accepted".into(),
                        started_ms: "1".into(),
                        finished_ms: None,
                    })
                }
                v::RequestBody::Query(v::Query::NetworkReport { operation_id, .. }) => {
                    let attempt = Attempt {
                        seq: 1,
                        rtt_ms: Some(1.0),
                        detail: "example.com. A 198.18.0.1".into(),
                        error: None,
                    };
                    let summary = Summary::from_attempts([&attempt]);
                    let items = [
                        Event::Attempt(attempt),
                        Event::Summary(summary),
                        Event::Done,
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, event)| v::NetworkResult {
                        id: (index + 1).to_string(),
                        sampled_at_ms: "1".into(),
                        data: v::browser_numbers(serde_json::to_value(event).unwrap()),
                    })
                    .collect();
                    v::ResponseBody::NetworkReport(v::NetworkReport {
                        operation_id,
                        schema_version: 1,
                        revision: "1".into(),
                        state: v::NetworkState::Complete,
                        message: "done".into(),
                        started_ms: "1".into(),
                        finished_ms: Some("2".into()),
                        truncated: false,
                        expired: false,
                        items,
                        cursor: "3".into(),
                        has_more: false,
                    })
                }
                other => panic!("unexpected request {other:?}"),
            };
            astra_management::client::send(&mut stream, &v::Response::success(request.id, body))
                .await
                .unwrap();
        }
    });
    let mut client = client();
    client.request_dns_query();
    client.request_dns_query();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            client.poll_dns_query(&socket);
            if !client.dns_query_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let query = client.dns_state.query.as_ref().unwrap();
    assert_eq!(
        query.summary.as_ref().unwrap().received,
        1,
        "{:?}",
        query.error
    );
    assert!(client
        .dns_query_output()
        .iter()
        .any(|row| row.text.contains("198.18.0.1")));
    assert_eq!(executions.load(Ordering::Relaxed), 1);
    stop.send(()).unwrap();
    server.await.unwrap();
}
