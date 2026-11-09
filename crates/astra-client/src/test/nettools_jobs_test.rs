use super::*;
use astra_management::v2 as v;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

fn idle_view() -> View {
    let mut view = View::new(
        Path::new("unused-network-page.sock"),
        "proxy".into(),
        &["DIRECT".into(), "remote-node".into()],
    );
    view.next_refresh = Instant::now() + Duration::from_secs(60);
    view
}
async fn poll_until(view: &mut View, predicate: impl Fn(&View) -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            view.poll();
            if predicate(view) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
fn hello() -> v::ResponseBody {
    v::ResponseBody::Hello(v::Hello {
        version: v::VERSION,
        role: "agent".into(),
        epoch: "remote-boot".into(),
        build_id: "fixture".into(),
        capabilities: vec![v::NETWORK_DIAGNOSTICS_CAPABILITY.into()],
    })
}
fn report(id: &str, state: v::NetworkState) -> v::NetworkReport {
    v::NetworkReport {
        operation_id: id.into(),
        schema_version: 1,
        revision: "1".into(),
        state,
        message: "fixture outcome".into(),
        started_ms: "1".into(),
        finished_ms: state.terminal().then(|| "2".into()),
        truncated: false,
        expired: false,
        items: vec![],
        cursor: "0".into(),
        has_more: false,
    }
}
async fn next(
    listener: &astra_platform::platform::LocalListener,
) -> (astra_platform::platform::LocalStream, v::Request) {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(4), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let request = astra_management::client::receive(&mut stream)
        .await
        .unwrap();
    (stream, request)
}
async fn reply(
    mut stream: astra_platform::platform::LocalStream,
    request: v::Request,
    body: v::ResponseBody,
) {
    astra_management::client::send(&mut stream, &v::Response::success(request.id, body))
        .await
        .unwrap();
}
async fn accept_job(listener: &astra_platform::platform::LocalListener, lost: bool) -> String {
    for _ in 0..1 {
        let (stream, request) = next(listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Hello)
        ));
        assert_eq!(request.agent_id.as_deref(), Some("remote-agent"));
        reply(stream, request, hello()).await;
    }
    let (stream, request) = next(listener).await;
    let id = match &request.body {
        v::RequestBody::Execute {
            operation_id,
            command: v::Command::NetworkDiagnostics { .. },
        } => operation_id.clone(),
        _ => panic!("Expected one execute"),
    };
    if lost {
        drop(stream);
    } else {
        reply(
            stream,
            request,
            v::ResponseBody::Task(v::Task {
                id: id.clone(),
                state: v::TaskState::Running,
                message: "accepted".into(),
                started_ms: "1".into(),
                finished_ms: None,
            }),
        )
        .await;
    }
    id
}
fn connected_view(socket: PathBuf) -> View {
    let mut view = idle_view();
    view.client =
        crate::network_diagnostics::Client::for_target(socket, Some("remote-agent".into()));
    view.target = "127.0.0.1:8080".into();
    view.count = 1;
    view
}

#[tokio::test]
async fn legacy_snapshot_shows_the_dns_limitation_before_any_execute() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("legacy-notice.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        let v::ResponseBody::Hello(mut metadata) = hello() else {
            unreachable!()
        };
        metadata.capabilities.clear();
        reply(stream, request, v::ResponseBody::Hello(metadata)).await;
        let (stream, request) = next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Runtime { .. })
        ));
        reply(
            stream,
            request,
            v::ResponseBody::Runtime(v::RuntimeSnapshot::Client(json!({}))),
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let mut view = connected_view(socket);
    view.next_refresh = Instant::now();
    poll_until(&mut view, |view| view.context.notice.is_some()).await;
    let notice = view.context.notice.as_deref().unwrap();
    assert!(
        notice.contains("系统域名解析路径不可确认")
            && notice.contains("bootstrap")
            && notice.contains("不使用客户端本机网卡")
    );
    assert!(view.operation_id.is_none() && !view.running());
    server.await.unwrap();
}

#[tokio::test]
async fn legacy_execute_is_sent_only_for_the_shared_safe_cases() {
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("network_eligibility_cases.json")).unwrap();
    for case in cases {
        let fixture = crate::import::tests::Fixture::new();
        let socket = fixture.0.join("legacy-admission.sock");
        let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
        let allowed = case["legacy"].as_bool().unwrap();
        let request: v::NetworkDiagnosticsRequest =
            serde_json::from_value(case["request"].clone()).unwrap();
        let expected = serde_json::to_value(&request.request).unwrap();
        let server = tokio::spawn(async move {
            let (stream, request) = next(&listener).await;
            let v::ResponseBody::Hello(mut metadata) = hello() else {
                unreachable!()
            };
            metadata.capabilities.clear();
            reply(stream, request, v::ResponseBody::Hello(metadata)).await;
            if allowed {
                let (stream, request) = next(&listener).await;
                let v::RequestBody::Execute {
                    command: v::Command::NetTool { request: sent, .. },
                    ..
                } = &request.body
                else {
                    panic!("expected legacy tool only")
                };
                assert_eq!(serde_json::to_value(sent).unwrap(), expected);
                reply(
                    stream,
                    request,
                    v::ResponseBody::NetTool(vec![json!({"event":"done"})]),
                )
                .await;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err(),
                "rejected requests never execute; successful legacy requests never replay"
            );
        });
        let (sender, mut updates) = mpsc::channel(16);
        let (_cancel, control) = tokio::sync::watch::channel(false);
        jobs::run(
            crate::network_diagnostics::Client::for_target(socket, None),
            "proxy".into(),
            request,
            sender,
            control,
            crate::waker::Waker::default(),
        )
        .await;
        let mut failed = false;
        while let Some(update) = updates.recv().await {
            failed |= matches!(update, jobs::Update::Error(_));
        }
        assert_eq!(failed, !allowed, "{}", case["name"]);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn single_and_batch_receipts_use_typed_codes_and_never_replay() {
    for batch in [false, true] {
        for (code, rejected) in [
            (Some(v::ErrorCode::Unsupported), true),
            (Some(v::ErrorCode::InvalidRequest), true),
            (Some(v::ErrorCode::Conflict), true),
            (Some(v::ErrorCode::OutcomeUnknown), false),
            (Some(v::ErrorCode::Offline), false),
            (Some(v::ErrorCode::Internal), false),
            (None, false),
        ] {
            let fixture = crate::import::tests::Fixture::new();
            let socket = fixture.0.join("typed-receipt.sock");
            let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let (stream, request) = next(&listener).await;
                let v::ResponseBody::Hello(mut metadata) = hello() else {
                    unreachable!()
                };
                metadata
                    .capabilities
                    .push(v::NETWORK_BATCH_CAPABILITY.into());
                reply(stream, request, v::ResponseBody::Hello(metadata)).await;
                let (mut stream, request) = next(&listener).await;
                let v::RequestBody::Execute {
                    operation_id,
                    command,
                } = &request.body
                else {
                    panic!("expected one execute")
                };
                assert_eq!(
                    matches!(command, v::Command::NetworkDiagnosticsBatch { .. }),
                    batch
                );
                let id = operation_id.clone();
                if let Some(code) = code {
                    astra_management::client::send(
                        &mut stream,
                        &v::Response::error(
                            request.id,
                            code,
                            "Unsupported: misleading receipt text",
                        ),
                    )
                    .await
                    .unwrap();
                }
                drop(stream);
                if !rejected {
                    let (stream, request) = next(&listener).await;
                    assert!(
                        matches!(&request.body, v::RequestBody::Query(v::Query::NetworkReport { operation_id, cursor: None }) if operation_id == &id)
                    );
                    reply(
                        stream,
                        request,
                        v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Complete)),
                    )
                    .await;
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "definite rejection must not query; unknown receipt must not replay"
                );
                id
            });
            let mut view = connected_view(socket);
            if batch {
                view.context.capabilities = vec![
                    v::NETWORK_DIAGNOSTICS_CAPABILITY.into(),
                    v::NETWORK_BATCH_CAPABILITY.into(),
                ];
                view.start_batch(v::NetworkBatchRequest {
                    projects: vec![v::NetworkBatchProject {
                        id: "tcp".into(),
                        path_id: "system".into(),
                        request: view.request().unwrap().into(),
                    }],
                    comparison: false,
                });
            } else {
                view.start();
            }
            poll_until(&mut view, |view| !view.running()).await;
            let id = server.await.unwrap();
            if rejected {
                assert!(view.operation_id.is_none() && view.report_state.is_none());
                assert!(view
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("misleading receipt text")));
            } else {
                assert_eq!(view.operation_id.as_deref(), Some(id.as_str()));
                assert_eq!(view.report_state, Some(v::NetworkState::Complete));
            }
        }
    }
}

#[tokio::test]
async fn legacy_icmp_cannot_bypass_tun_awareness_or_send_an_execute() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("legacy-icmp.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Hello)
        ));
        let v::ResponseBody::Hello(mut metadata) = hello() else {
            unreachable!()
        };
        metadata.capabilities.clear();
        reply(stream, request, v::ResponseBody::Hello(metadata)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "legacy ICMP must be rejected before Execute"
        );
    });
    let mut view = connected_view(socket);
    view.set_kind(Kind::Icmp);
    view.target = "127.0.0.1".into();
    view.start();
    poll_until(&mut view, |view| !view.running()).await;
    assert!(view
        .error
        .as_deref()
        .is_some_and(|error| error.contains("TUN") && error.contains("未启动探测")));
    assert!(view.attempts.is_empty() && view.summary.is_none() && view.operation_id.is_none());
    server.await.unwrap();
}

#[test]
fn fake_ip_input_is_bounded_and_readonly_tabs_do_not_start_jobs() {
    assert!(lookup::addresses("example.test").is_err());
    assert!(lookup::addresses(&vec!["127.0.0.1"; 65].join(" ")).is_err());
    assert_eq!(
        lookup::addresses("198.18.0.1, ::1").unwrap(),
        vec!["198.18.0.1", "::1"]
    );
    let mut view = idle_view();
    view.section = Section::Overview;
    view.start();
    assert!(!view.running());
    view.details = vec![json!({"large":"x".repeat(2 * 1024 * 1024)})];
    assert!(view.export_json().is_err());
}

#[tokio::test]
async fn earlier_job_agent_cannot_silently_ignore_explicit_resolver() {
    let directory = crate::import::tests::Fixture::new();
    let socket = directory.0.join("old-jobs.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Hello)
        ));
        reply(stream, request, hello()).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(80), listener.accept())
                .await
                .is_err()
        );
    });
    let mut view = connected_view(socket);
    view.resolver = "udp://127.0.0.1:5353".into();
    view.start();
    poll_until(&mut view, |view| !view.running()).await;
    assert!(view
        .error
        .as_deref()
        .unwrap()
        .contains("未声明约束解析能力"));
    assert!(view.operation_id.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn resume_only_reads_original_report_and_keeps_terminal_evidence() {
    let directory = crate::import::tests::Fixture::new();
    let socket = directory.0.join("resume.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body, v::RequestBody::Query(v::Query::NetworkReport { operation_id, cursor: None }) if operation_id == "original-job")
        );
        assert_eq!(request.agent_id.as_deref(), Some("remote-agent"));
        let mut page = report("original-job", v::NetworkState::Cancelled);
        page.message = "cancelled with retained evidence".into();
        reply(stream, request, v::ResponseBody::NetworkReport(page)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(80), listener.accept())
                .await
                .is_err()
        );
    });
    let mut view = connected_view(socket);
    view.kind = Kind::Report;
    view.target = "original-job".into();
    view.start();
    poll_until(&mut view, |view| !view.running()).await;
    assert_eq!(view.operation_id.as_deref(), Some("original-job"));
    assert_eq!(view.report_state, Some(v::NetworkState::Cancelled));
    assert!(view
        .export_json()
        .unwrap()
        .contains("cancelled with retained evidence"));
    server.await.unwrap();
}

#[tokio::test]
async fn fake_ip_missing_capability_does_not_send_lookup_or_probe() {
    let directory = crate::import::tests::Fixture::new();
    let socket = directory.0.join("fake.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        let v::ResponseBody::Hello(mut h) = hello() else {
            unreachable!()
        };
        h.capabilities.push("core_network_v1".into());
        reply(stream, request, v::ResponseBody::Hello(h)).await;
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body, v::RequestBody::Query(v::Query::NetworkCore { instance }) if instance == "proxy")
        );
        reply(
            stream,
            request,
            v::ResponseBody::NetworkCore(v::NetworkCoreReply {
                schema_version: 1,
                instance: "proxy".into(),
                sampled_at_ms: "1".into(),
                status: v::CoreQueryStatus::Ok,
                message: None,
                identity: Some(v::NetworkCoreIdentity {
                    version: 1,
                    boot_id: "boot".into(),
                    pid: 1,
                    applied_digest: None,
                    capabilities: None,
                }),
            }),
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(80), listener.accept())
                .await
                .is_err()
        );
    });
    let mut view = connected_view(socket);
    view.kind = Kind::FakeIp;
    view.target = "198.18.0.1".into();
    view.start();
    poll_until(&mut view, |view| !view.running()).await;
    assert!(view.error.as_deref().unwrap().contains("能力无法确认"));
    assert!(view.operation_id.is_none());
    server.await.unwrap();
}

#[test]
fn new_page_never_populates_egresses_with_client_local_interfaces() {
    let view = idle_view();
    assert_eq!(
        view.egresses,
        vec![
            Egress::System,
            Egress::Outbound("DIRECT".into()),
            Egress::Outbound("remote-node".into())
        ]
    );
}

#[tokio::test]
async fn only_target_snapshot_supplies_interfaces_and_missing_selection_never_falls_back() {
    let mut view = idle_view();
    let context = jobs::Context {
        capabilities: vec!["network_state_v1".into()],
        network: Some(json!({"agent_id":"remote-agent","interfaces":[
            {"id":"remote-42","name":"Remote Ethernet","connected":true,"kind":"physical"},
            {"id":"remote-down","connected":false,"kind":"physical"},
            {"id":"remote-loopback","connected":true,"kind":"loopback"}
        ]})),
        ..Default::default()
    };
    let (tx, rx) = mpsc::channel(1);
    tx.send(context).await.unwrap();
    view.context_receiver = Some(rx);
    view.poll();
    assert_eq!(
        view.egresses
            .iter()
            .filter(|v| matches!(v, Egress::Interface(_)))
            .collect::<Vec<_>>(),
        vec![&Egress::Interface("remote-42".into())]
    );
    view.egress = view.egresses.len() - 1;
    let (tx, rx) = mpsc::channel(1);
    tx.send(jobs::Context {
        notice: Some("Target Agent unavailable".into()),
        capabilities: vec!["network_state_v1".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    view.context_receiver = Some(rx);
    view.poll();
    assert_eq!(
        view.egresses[view.egress],
        Egress::Interface("remote-42".into())
    );
    assert!(view.request().unwrap_err().contains("无法确认"));
    assert!(!view.running());
}

#[test]
fn dns_details_keep_flags_sections_ttl_and_full_bounded_record_text() {
    let mut view = idle_view();
    view.show_advanced = true;
    let text = format!("{}-last-marker", "x".repeat(8000));
    view.details = vec![
        json!({"event":"dns","seq":"1","detail":{"kind":"response","outcome":"resolved","flags":{"authenticated_data":true},"dnssec_locally_validated":false,"edns":{"udp_payload":"1232"}}}),
        json!({"event":"dns","seq":"1","detail":{"kind":"records","section":"authority","records":[{"name":"example.test.","class":"IN","record_type":"TXT","ttl":"123","text":text,"text_truncated":true}]}}),
    ];
    let output = view.output().join("\n");
    for expected in [
        "authenticated_data",
        "dnssec_locally_validated: false",
        "1232",
        "authority",
        "TTL=123",
        "-last-marker",
        "已截断",
    ] {
        assert!(output.contains(expected), "missing {expected}");
    }
    assert!(output.contains(&"x".repeat(8000)));
}

#[test]
fn four_sections_and_scroll_keys_preserve_the_form_without_starting_probes() {
    let mut view = idle_view();
    let target = view.target.clone();
    view.section = Section::Findings;
    for section in Section::ALL {
        assert!(view.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)));
        assert!(view.section == section);
        if section != Section::Tools {
            assert!(view.fields().is_empty());
            assert!(!view.output().is_empty());
        }
        assert!(!view.running());
    }
    assert!(view.key(KeyEvent::from(KeyCode::PageUp)));
    assert_eq!(view.scroll, 10);
    assert!(view.key(KeyEvent::from(KeyCode::PageDown)));
    assert_eq!(view.scroll, 0);
    assert_eq!(view.target, target);
}

#[tokio::test]
async fn stop_sends_cancel_and_waits_for_agent_terminal_report() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("jobs.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let (cancel_seen, seen) = oneshot::channel();
    let (release, cleanup) = oneshot::channel();
    let server = tokio::spawn(async move {
        let id = accept_job(&listener, false).await;
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body,v::RequestBody::Query(v::Query::NetworkReport {operation_id,..}) if operation_id==&id)
        );
        reply(
            stream,
            request,
            v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Running)),
        )
        .await;
        let (stream, request) = loop {
            let (stream, request) = next(&listener).await;
            if matches!(&request.body, v::RequestBody::Query(v::Query::NetworkReport { operation_id, .. }) if operation_id == &id)
            {
                reply(
                    stream,
                    request,
                    v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Running)),
                )
                .await;
                continue;
            }
            assert!(
                matches!(&request.body, v::RequestBody::Execute { operation_id: cancel_id, command: v::Command::CancelTask { operation_id } } if operation_id == &id && cancel_id != &id)
            );
            break (stream, request);
        };
        cancel_seen.send(()).unwrap();
        cleanup.await.unwrap();
        reply(
            stream,
            request,
            v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Cancelled)),
        )
        .await;
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body,v::RequestBody::Query(v::Query::NetworkReport {operation_id,..}) if operation_id==&id)
        );
        reply(
            stream,
            request,
            v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Cancelled)),
        )
        .await;
    });
    let mut view = connected_view(socket);
    view.start();
    poll_until(&mut view, |v| {
        v.report_state == Some(v::NetworkState::Running)
    })
    .await;
    view.stop();
    tokio::time::timeout(Duration::from_secs(3), seen)
        .await
        .unwrap()
        .unwrap();
    view.poll();
    assert!(view.running());
    assert_eq!(view.report_state, Some(v::NetworkState::Running));
    release.send(()).unwrap();
    poll_until(&mut view, |v| !v.running()).await;
    assert_eq!(view.report_state, Some(v::NetworkState::Cancelled));
    server.await.unwrap();
}

#[tokio::test]
async fn closing_the_page_stops_observation_without_sending_cancel() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("jobs.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let (ready, closed) = oneshot::channel();
    let server = tokio::spawn(async move {
        let id = accept_job(&listener, false).await;
        let (stream, request) = next(&listener).await;
        reply(
            stream,
            request,
            v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Running)),
        )
        .await;
        closed.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), listener.accept())
                .await
                .is_err(),
            "Closing the page must not send CancelTask"
        );
    });
    let mut view = connected_view(socket);
    view.start();
    poll_until(&mut view, |v| {
        v.report_state == Some(v::NetworkState::Running)
    })
    .await;
    let retained_id = view.operation_id.clone();
    assert!(retained_id.is_some());
    drop(view);
    ready.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn lost_acceptance_and_report_replies_query_original_identity_without_reexecute() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("jobs.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let id = accept_job(&listener, true).await;
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body,v::RequestBody::Query(v::Query::NetworkReport {operation_id,cursor:None}) if operation_id==&id)
        );
        drop(stream);
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body,v::RequestBody::Query(v::Query::NetworkReport {operation_id,cursor:None}) if operation_id==&id)
        );
        let mut report = report(&id, v::NetworkState::Complete);
        report.items.push(v::NetworkResult {id:"1".into(),sampled_at_ms:"2".into(),data:json!({"event":"dns","seq":"1","detail":{"kind":"response","outcome":"resolved"}})});
        report.cursor = "1".into();
        reply(stream, request, v::ResponseBody::NetworkReport(report)).await;
        id
    });
    let mut view = connected_view(socket);
    view.start();
    poll_until(&mut view, |v| !v.running()).await;
    assert_eq!(view.report_state, Some(v::NetworkState::Complete));
    assert!(view.error.is_none());
    assert_eq!(view.details.len(), 1);
    assert_eq!(view.operation_id, Some(server.await.unwrap()));
}

#[tokio::test]
async fn stop_while_acceptance_reply_is_pending_queries_and_cancels_the_persisted_job() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("jobs.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let (persisted, accepted) = oneshot::channel();
    let (cancel_seen, seen) = oneshot::channel();
    let (release, cleanup) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Hello)
        ));
        reply(stream, request, hello()).await;
        let (held_acceptance, request) = next(&listener).await;
        let id = match &request.body {
            v::RequestBody::Execute {
                operation_id,
                command: v::Command::NetworkDiagnostics { .. },
            } => operation_id.clone(),
            _ => panic!("Expected one execute"),
        };
        persisted.send(id.clone()).unwrap();
        // Keep the original connection open without sending its receipt. Stop
        // must issue a separate report query rather than await this connection.
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body,v::RequestBody::Query(v::Query::NetworkReport {operation_id,..}) if operation_id==&id)
        );
        reply(
            stream,
            request,
            v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Running)),
        )
        .await;
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body,v::RequestBody::Execute {operation_id:cancel_id,command:v::Command::CancelTask {operation_id}} if operation_id==&id && cancel_id!=&id)
        );
        cancel_seen.send(()).unwrap();
        cleanup.await.unwrap();
        reply(
            stream,
            request,
            v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Cancelled)),
        )
        .await;
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body,v::RequestBody::Query(v::Query::NetworkReport {operation_id,..}) if operation_id==&id)
        );
        reply(
            stream,
            request,
            v::ResponseBody::NetworkReport(report(&id, v::NetworkState::Cancelled)),
        )
        .await;
        drop(held_acceptance);
    });
    let mut view = connected_view(socket);
    view.start();
    let id = tokio::time::timeout(Duration::from_secs(3), accepted)
        .await
        .unwrap()
        .unwrap();
    poll_until(&mut view, |v| {
        v.operation_id.as_deref() == Some(id.as_str())
    })
    .await;
    assert_eq!(view.report_state, None);
    view.stop();
    tokio::time::timeout(Duration::from_secs(3), seen)
        .await
        .unwrap()
        .unwrap();
    view.poll();
    assert!(view.running());
    assert_eq!(view.report_state, Some(v::NetworkState::Running));
    release.send(()).unwrap();
    poll_until(&mut view, |v| !v.running()).await;
    assert_eq!(view.report_state, Some(v::NetworkState::Cancelled));
    server.await.unwrap();
}
