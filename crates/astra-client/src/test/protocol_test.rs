use super::*;

fn runtime_wire_fixture() -> astra_model::runtime::RuntimeView {
    use astra_model::runtime::*;
    RuntimeView {
        snapshot: Snapshot {
            pid: 123,
            upload: 9_007_199_254_740_993,
            download: u64::MAX,
            connections: vec![Connection {
                id: 12,
                source: Some("123".into()),
                host: "456".into(),
                port: 53,
                network: "udp".into(),
                inbound: "789".into(),
                outbound: "12".into(),
                upload: 13,
                download: 14,
                elapsed_ms: 15,
            }],
            groups: vec![Group {
                name: "12".into(),
                mode: "select".into(),
                selected: Some("123".into()),
                testing: false,
                members: vec![Member {
                    name: "123".into(),
                    delay_ms: Some(12),
                    alive: Some(true),
                }],
            }],
            ..Default::default()
        },
        telemetry: Some(RuntimeTelemetry {
            host_boot_id: "123".into(),
            active_generation: Some(12),
            coverage: Coverage::Complete,
            sample_sequence: 13,
            sample_elapsed_ms: 14,
            sample_age_ms: 15,
            generations: vec![
                GenerationTelemetry {
                    generation: 12,
                    sample_age_ms: Some(13),
                    healthy: true,
                },
                GenerationTelemetry {
                    generation: 11,
                    sample_age_ms: None,
                    healthy: false,
                },
            ],
            connection_generations: [(12, 13)].into_iter().collect(),
            dns: Some(Box::new(DnsQueryLog {
                records: vec![DnsQuery {
                    resolver: "123".into(),
                    id: u64::MAX,
                    timestamp_ms: 1_800_000_000_012,
                    domain: "123".into(),
                    query_type: "TXT".into(),
                    source: "cache".into(),
                    upstream: Some("1.1.1.1:53".into()),
                    status: "NoError".into(),
                    answers: vec!["12".into(), "9007199254740993".into()],
                    duration_us: 12,
                    ..Default::default()
                }],
                truncated: false,
            })),
            dns_resolver: Some(Box::new(DnsResolverStatus {
                enabled: true,
                fake_ip: true,
                listen: vec!["127.0.0.1:1053".parse().unwrap()],
                policy_count: 12,
                nameservers: vec![DnsServer {
                    address: "https://resolver.example/dns-query".into(),
                    transport: "DoH".into(),
                }],
                ..Default::default()
            })),
            traffic: Some(Box::new(TrafficTelemetry {
                nodes: Some(
                    [(
                        "12".into(),
                        NodeTraffic {
                            session: 12,
                            hour: 13,
                            day: 14,
                            week: 15,
                            month: 9_007_199_254_740_993,
                            persist: u64::MAX,
                        },
                    )]
                    .into_iter()
                    .collect(),
                ),
                selected_node: Some("12".into()),
                history_error: None,
            })),
            ..Default::default()
        }),
    }
}

#[test]
fn runtime_v2_roundtrip_preserves_dns_and_traffic_counters_and_numeric_text() {
    use astra_model::runtime::RuntimeView;
    let populated = runtime_wire_fixture();
    let mut empty = populated.clone();
    empty
        .telemetry
        .as_mut()
        .unwrap()
        .dns
        .as_mut()
        .unwrap()
        .records
        .clear();
    let legacy = RuntimeView {
        snapshot: populated.snapshot.clone(),
        telemetry: None,
    };
    for view in [populated, empty, legacy] {
        let native = serde_json::to_value(&view).unwrap();
        for wire in [native.clone(), v::browser_numbers(native.clone())] {
            let response =
                reply(v::ResponseBody::Runtime(v::RuntimeSnapshot::Client(wire))).unwrap();
            let decoded: RuntimeView = serde_json::from_str(&response.text).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), native);
        }
    }
    let mut wire = v::browser_numbers(serde_json::to_value(runtime_wire_fixture()).unwrap());
    wire["telemetry"]["dns"]["records"][0]["duration_us"] = "18446744073709551616".into();
    let response = reply(v::ResponseBody::Runtime(v::RuntimeSnapshot::Client(wire))).unwrap();
    assert!(
        serde_json::from_str::<RuntimeView>(&response.text).is_err(),
        "overflow must remain an error instead of becoming zero or rounding"
    );
}

#[tokio::test]
async fn runtime_subscription_decodes_browser_formatted_dns_records() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("runtime-v2.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let expected = serde_json::to_value(runtime_wire_fixture()).unwrap();
    let wire = v::browser_numbers(expected.clone());
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: v::Request = astra_management::client::receive(&mut stream)
                .await
                .unwrap();
            assert!(
                matches!(request.body, v::RequestBody::Query(v::Query::Runtime { instance }) if instance == "dns-profile")
            );
            astra_management::client::send(
                &mut stream,
                &v::Response::success(
                    request.id,
                    v::ResponseBody::Runtime(v::RuntimeSnapshot::Client(wire.clone())),
                ),
            )
            .await
            .unwrap();
        }
    });
    let mut stream = subscribe(
        &socket,
        Action::WatchRuntime {
            name: "dns-profile".into(),
            expected_pid: 123,
            interval_ms: 500,
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let response: Reply = tokio::time::timeout(
            Duration::from_secs(3),
            astra_ipc::receive_management(&mut stream),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        let decoded: astra_model::runtime::RuntimeView =
            serde_json::from_str(&response.text).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
    }
    drop(stream);
    server.await.unwrap();
}

#[tokio::test]
#[ignore = "read-only check against ASTRA_RUNTIME_TEST_SOCKET and ASTRA_RUNTIME_TEST_INSTANCE"]
async fn live_runtime_subscription_decodes_dns_metadata() {
    let socket = PathBuf::from(
        std::env::var_os("ASTRA_RUNTIME_TEST_SOCKET").expect("management socket required"),
    );
    let name = std::env::var("ASTRA_RUNTIME_TEST_INSTANCE").expect("instance required");
    let response = reply(
        query(
            &socket,
            v::Query::Runtime {
                instance: name.clone(),
            },
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let first: astra_model::runtime::RuntimeView = serde_json::from_str(&response.text).unwrap();
    let mut stream = subscribe(
        &socket,
        Action::WatchRuntime {
            name,
            expected_pid: first.snapshot.pid,
            interval_ms: 500,
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let response: Reply = tokio::time::timeout(
            Duration::from_secs(5),
            astra_ipc::receive_management(&mut stream),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        let view: astra_model::runtime::RuntimeView = serde_json::from_str(&response.text).unwrap();
        let meta = view.telemetry.expect("enhanced runtime telemetry required");
        let records = meta.dns.expect("DNS history required");
        assert!(
            !records.records.is_empty(),
            "generate a DNS query before running this read-only check"
        );
        assert!(meta.dns_resolver.is_some());
        println!("Decoded live DNS sample: {} records", records.records.len());
    }
}

#[test]
fn force_start_is_explicit_for_local_and_remote_targets() {
    for local in [true, false] {
        assert!(matches!(
            call_plan(Action::ForceStart("proxy".into()), local).unwrap(),
            CallPlan::Execute {
                command: v::Command::ForceStart { instance }, ..
            } if instance == "proxy"
        ));
        assert!(matches!(
            call_plan(Action::Start("proxy".into()), local).unwrap(),
            CallPlan::Execute {
                command: v::Command::Start { instance }, ..
            } if instance == "proxy"
        ));
    }
}

#[test]
fn monitor_is_a_read_only_query_for_local_and_remote_targets() {
    for local in [true, false] {
        assert!(matches!(
            call_plan(Action::Monitor { name: "core".into() }, local).unwrap(),
            CallPlan::Request(v::RequestBody::Query(v::Query::Monitor { instance })) if instance == "core"
        ));
    }
}

#[test]
fn monitor_reply_maps_durable_v2_identity_to_the_tui_request_name() {
    let snapshot = v::MonitorSnapshot {
        instance: "cf85e3bf-f8e4-4090-b8c3-69193a0e1ff6".into(),
        pid: Some(42),
        memory_bytes: Some("9007199254740993".into()),
        ..Default::default()
    };
    let response = reply_for_action(v::ResponseBody::Monitor(snapshot), Some("proxy")).unwrap();
    let sample: v::MonitorSnapshot = serde_json::from_str(&response.text).unwrap();
    assert_eq!(sample.instance, "proxy");
    assert_eq!(sample.pid, Some(42));
    assert_eq!(sample.memory_bytes.as_deref(), Some("9007199254740993"));
}

#[test]
fn update_transaction_retains_the_callers_operation_identity() {
    let request = updates::UpdateRequest::new(updates::UpdateIntent::Rollback {
        target_directory: PathBuf::from("chosen-release"),
    });
    let expected = request.operation_id.clone();
    let CallPlan::Execute {
        operation_id,
        command: v::Command::Update { request },
    } = call_plan(Action::UpdateTransaction { request }, true).unwrap()
    else {
        panic!("An explicit update transaction must use the durable Execute path");
    };
    assert_eq!(operation_id, expected);
    assert_eq!(request.operation_id, expected);
    assert!(
        matches!(request.intent, updates::UpdateIntent::Rollback { target_directory }
        if target_directory == Path::new("chosen-release"))
    );
}

#[test]
fn save_and_rollback_defer_their_state_checks_until_execution() {
    let CallPlan::Save {
        name,
        original,
        text,
    } = call_plan(
        Action::Save {
            name: "proxy".into(),
            original: "expected source revision".into(),
            text: "edited document".into(),
        },
        true,
    )
    .unwrap()
    else {
        panic!("Saving must retain the original document for the draft conflict check");
    };
    assert_eq!(name, "proxy");
    assert_eq!(original, "expected source revision");
    assert_eq!(text, "edited document");
    assert!(matches!(
        call_plan(Action::Rollback, true).unwrap(),
        CallPlan::Rollback
    ));
}

#[test]
fn local_tun_enable_keeps_elevation_off_the_remote_execute_path() {
    let action = Action::Runtime {
        name: "proxy".into(),
        command: astra_model::runtime::Command::Tun(true),
        expected_pid: Some(123),
    };
    assert!(matches!(
        call_plan(action.clone(), true).unwrap(),
        CallPlan::Request(v::RequestBody::Local(v::LocalCommand::EnableTun {
            instance, expected_pid: Some(123)
        })) if instance == "proxy"
    ));
    assert!(matches!(
        call_plan(action, false).unwrap(),
        CallPlan::Execute { command: v::Command::Runtime {
            instance, command: v::RuntimeCommand::Tun(true), expected_pid: Some(123)
        }, .. } if instance == "proxy"
    ));
    assert!(matches!(
        call_plan(
            Action::Runtime {
                name: "proxy".into(),
                command: astra_model::runtime::Command::Tun(false),
                expected_pid: Some(123),
            },
            true
        )
        .unwrap(),
        CallPlan::Execute {
            command: v::Command::Runtime {
                command: v::RuntimeCommand::Tun(false),
                ..
            },
            ..
        }
    ));
}

#[test]
fn subscription_only_actions_cannot_be_sent_as_single_requests() {
    for action in [
        Action::Session,
        Action::FollowLogs {
            name: "proxy".into(),
            cursor: None,
        },
    ] {
        let Err(error) = call_plan(action, true) else {
            panic!("Subscription-only actions require the stream adapter");
        };
        assert!(error.to_string().contains("stream adapter"));
    }
}
