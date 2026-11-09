use super::*;
use crate::nettool::nat_fixture as fixture;
use std::{cell::Cell, net::Ipv4Addr};
struct LocalGuard {
    checks: Cell<usize>,
    fail_after: usize,
}
impl Guard for LocalGuard {
    fn bind(&self) -> std::io::Result<UdpSocket> {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
        socket.set_nonblocking(true)?;
        UdpSocket::from_std(socket)
    }
    async fn check(&self, _: SocketAddr) -> Result<()> {
        self.checks.set(self.checks.get() + 1);
        ensure!(
            self.checks.get() <= self.fail_after,
            "fixture interface removed"
        );
        Ok(())
    }
    fn evidence(&self) -> Option<astra_model::network_path::PhysicalPathEvidence> {
        let valid = self.checks.get() <= self.fail_after;
        Some(astra_model::network_path::PhysicalPathEvidence {
            interface_id: "fixture".into(),
            interface_name: "fixture".into(),
            interface_index: 0,
            source_address: "127.0.0.1".into(),
            family: "ipv4".into(),
            sampled_at_ms: self.checks.get().to_string(),
            binding_verified: valid,
            route_verified: valid,
            target: None,
            method: "test-only guard".into(),
        })
    }
}
fn report() -> Report {
    Report {
        interface_id: "fixture".into(),
        interface_name: "fixture".into(),
        family: model::AddressFamily::Ipv4,
        local_endpoint: None,
        physical_path: None,
        comparison: Comparison::Insufficient,
        mapping: Behavior::Unknown,
        filtering: Behavior::Unknown,
        traditional_type: None,
        advanced_capable: false,
        unstable: false,
        partial: false,
        elapsed_ms: 0,
        observations: vec![],
        notes: vec![],
    }
}
#[tokio::test]
async fn all_nine_mapping_filtering_combinations_use_real_unconnected_udp() {
    for mapping in [
        Behavior::EndpointIndependent,
        Behavior::AddressDependent,
        Behavior::AddressAndPortDependent,
    ] {
        for filtering in [
            Behavior::EndpointIndependent,
            Behavior::AddressDependent,
            Behavior::AddressAndPortDependent,
        ] {
            let fixture = fixture::start(fixture::Config {
                mapping,
                filtering,
                ..Default::default()
            })
            .await;
            let guard = LocalGuard {
                checks: Cell::new(0),
                fail_after: usize::MAX,
            };
            let socket = guard.bind().unwrap();
            assert!(socket.peer_addr().is_err());
            let mut report = report();
            let retries = [Duration::from_millis(8), Duration::from_millis(12)];
            let emit = |_: ToolEvent| Ok(());
            let mut engine = Engine {
                guard: &guard,
                report: &mut report,
                emit: &emit,
                observed: HashMap::new(),
                retries: &retries,
                next_transaction: tokio::time::Instant::now(),
            };
            let answers = engine
                .basic(&socket, &fixture.endpoints[..1])
                .await
                .unwrap();
            engine
                .advanced(&socket, answers[0].0, &answers[0].1)
                .await
                .unwrap();
            assert_eq!(
                report.mapping, mapping,
                "mapping for {mapping:?}/{filtering:?}"
            );
            assert_eq!(
                report.filtering, filtering,
                "filter for {mapping:?}/{filtering:?}"
            );
            let expected_type = match (mapping, filtering) {
                (Behavior::EndpointIndependent, Behavior::EndpointIndependent) => {
                    Some("full_cone_observed")
                }
                (Behavior::EndpointIndependent, Behavior::AddressDependent) => {
                    Some("restricted_cone_observed")
                }
                (Behavior::EndpointIndependent, Behavior::AddressAndPortDependent) => {
                    Some("port_restricted_cone_observed")
                }
                (Behavior::AddressAndPortDependent, Behavior::AddressAndPortDependent) => {
                    Some("symmetric_observed")
                }
                _ => None,
            };
            assert_eq!(report.traditional_type.as_deref(), expected_type);
            assert!(report.advanced_capable);
            assert!(!report.unstable);
            let port = socket.local_addr().unwrap().port();
            assert!(report
                .observations
                .iter()
                .filter(|o| o.phase.starts_with("mapping") || o.phase.starts_with("basic"))
                .all(|o| o.local_endpoint.parse::<SocketAddr>().unwrap().port() == port));
            let filtering = report
                .observations
                .iter()
                .find(|o| o.phase == "filtering_initial")
                .unwrap()
                .local_endpoint
                .parse::<SocketAddr>()
                .unwrap();
            assert_ne!(filtering.port(), port);
            assert_eq!(
                fixture.contacts.lock().unwrap()[&filtering].len(),
                1,
                "filtering session must never prime alternate endpoints"
            );
        }
    }
}
#[tokio::test]
async fn ordinary_mapping_differences_are_not_instability_or_full_nat_type() {
    let fixture = fixture::start(fixture::Config {
        mapping: Behavior::AddressAndPortDependent,
        ..Default::default()
    })
    .await;
    let guard = LocalGuard {
        checks: Cell::new(0),
        fail_after: usize::MAX,
    };
    let socket = guard.bind().unwrap();
    let mut report = report();
    let emit = |_: ToolEvent| Ok(());
    let retries = [Duration::from_millis(50)];
    let mut engine = Engine {
        guard: &guard,
        report: &mut report,
        emit: &emit,
        observed: HashMap::new(),
        retries: &retries,
        next_transaction: tokio::time::Instant::now(),
    };
    engine.basic(&socket, &fixture.endpoints).await.unwrap();
    assert_eq!(report.comparison, Comparison::Different);
    assert!(!report.unstable);
    assert_eq!(report.mapping, Behavior::Unknown);
    assert!(report.traditional_type.is_none());
}
#[tokio::test]
async fn same_target_remap_is_unstable_and_withholds_classification() {
    let fixture = fixture::start(fixture::Config {
        unstable: true,
        ..Default::default()
    })
    .await;
    let guard = LocalGuard {
        checks: Cell::new(0),
        fail_after: usize::MAX,
    };
    let socket = guard.bind().unwrap();
    let mut report = report();
    let emit = |_: ToolEvent| Ok(());
    let retries = [Duration::from_millis(30)];
    let mut engine = Engine {
        guard: &guard,
        report: &mut report,
        emit: &emit,
        observed: HashMap::new(),
        retries: &retries,
        next_transaction: tokio::time::Instant::now(),
    };
    engine
        .basic(&socket, &fixture.endpoints[..1])
        .await
        .unwrap();
    assert!(report.unstable);
    assert_eq!(report.comparison, Comparison::Unstable);
    assert!(report.traditional_type.is_none());
}
#[tokio::test]
async fn change_request_must_be_verified_before_any_filtering_classification() {
    let fixture = fixture::start(fixture::Config {
        supports_change: false,
        ..Default::default()
    })
    .await;
    let guard = LocalGuard {
        checks: Cell::new(0),
        fail_after: usize::MAX,
    };
    let socket = guard.bind().unwrap();
    let mut report = report();
    let emit = |_: ToolEvent| Ok(());
    let retries = [Duration::from_millis(8)];
    let mut engine = Engine {
        guard: &guard,
        report: &mut report,
        emit: &emit,
        observed: HashMap::new(),
        retries: &retries,
        next_transaction: tokio::time::Instant::now(),
    };
    let answers = engine
        .basic(&socket, &fixture.endpoints[..1])
        .await
        .unwrap();
    assert!(engine
        .advanced(&socket, answers[0].0, &answers[0].1)
        .await
        .is_err());
    assert_eq!(report.filtering, Behavior::Unknown);
    assert!(!report.advanced_capable);
    assert!(report.traditional_type.is_none());
}

#[test]
fn behavior_candidate_prefers_a_complete_advertisement_over_basic_stun() {
    let primary = "192.0.2.1:3478".parse().unwrap();
    let basic = stun::Binding {
        mapped: "198.51.100.1:52000".parse().unwrap(),
        origin: None,
        other: None,
    };
    let mut invalid = basic.clone();
    invalid.origin = Some(primary);
    invalid.other = Some("192.0.2.1:3479".parse().unwrap());
    let mut capable = invalid.clone();
    capable.other = Some("192.0.2.2:3479".parse().unwrap());
    let answers = [(primary, basic), (primary, invalid), (primary, capable)];
    assert_eq!(
        behavior_candidate(&answers).unwrap().1.other,
        answers[2].1.other
    );
    assert!(behavior_candidate(&[]).is_none());
    assert!(behavior_candidate(&answers[..1]).unwrap().1.other.is_none());
}

#[tokio::test]
async fn later_behavior_service_still_identifies_type_after_a_basic_only_service() {
    let basic = fixture::start(fixture::Config {
        advertise_behavior: false,
        ..Default::default()
    })
    .await;
    let capable = fixture::start(Default::default()).await;
    let guard = LocalGuard {
        checks: Cell::new(0),
        fail_after: usize::MAX,
    };
    let socket = guard.bind().unwrap();
    let mut report = report();
    let emit = |_: ToolEvent| Ok(());
    let retries = [Duration::from_millis(50)];
    let mut engine = Engine {
        guard: &guard,
        report: &mut report,
        emit: &emit,
        observed: HashMap::new(),
        retries: &retries,
        next_transaction: tokio::time::Instant::now(),
    };
    let answers = engine
        .basic(&socket, &[basic.endpoints[0], capable.endpoints[0]])
        .await
        .unwrap();
    assert_eq!(answers.len(), 2);
    assert!(engine
        .advanced(&socket, answers[0].0, &answers[0].1)
        .await
        .is_err());
    assert!(!engine.report.advanced_capable);
    assert!(engine.report.traditional_type.is_none());
    let (primary, initial) = behavior_candidate(&answers).unwrap();
    assert_eq!(*primary, capable.endpoints[0]);
    engine.advanced(&socket, *primary, initial).await.unwrap();
    assert_eq!(
        report.traditional_type.as_deref(),
        Some("full_cone_observed")
    );
}

#[tokio::test]
async fn untranslated_paths_are_not_labeled_cone_nat_even_when_filtered() {
    for filtering in [
        Behavior::EndpointIndependent,
        Behavior::AddressDependent,
        Behavior::AddressAndPortDependent,
    ] {
        let fixture = fixture::start(fixture::Config {
            translate: false,
            filtering,
            ..Default::default()
        })
        .await;
        let guard = LocalGuard {
            checks: Cell::new(0),
            fail_after: usize::MAX,
        };
        let socket = guard.bind().unwrap();
        let mut report = report();
        let emit = |_: ToolEvent| Ok(());
        let retries = [Duration::from_millis(20)];
        let mut engine = Engine {
            guard: &guard,
            report: &mut report,
            emit: &emit,
            observed: HashMap::new(),
            retries: &retries,
            next_transaction: tokio::time::Instant::now(),
        };
        let answers = engine
            .basic(&socket, &fixture.endpoints[..1])
            .await
            .unwrap();
        engine
            .advanced(&socket, answers[0].0, &answers[0].1)
            .await
            .unwrap();
        assert_eq!(
            report.traditional_type.as_deref(),
            Some("no_translation_observed")
        );
        assert_eq!(report.filtering, filtering);
    }
}

#[tokio::test]
async fn rejected_or_invalid_filtering_responses_do_not_produce_nat_types() {
    for fault in [
        fixture::FilteringFault::Rejected,
        fixture::FilteringFault::InvalidPackets,
    ] {
        let fixture = fixture::start(fixture::Config {
            mapping: Behavior::AddressAndPortDependent,
            filtering_fault: Some(fault),
            ..Default::default()
        })
        .await;
        let guard = LocalGuard {
            checks: Cell::new(0),
            fail_after: usize::MAX,
        };
        let socket = guard.bind().unwrap();
        let mut report = report();
        let emit = |_: ToolEvent| Ok(());
        let retries = [Duration::from_millis(50)];
        let mut engine = Engine {
            guard: &guard,
            report: &mut report,
            emit: &emit,
            observed: HashMap::new(),
            retries: &retries,
            next_transaction: tokio::time::Instant::now(),
        };
        let answers = engine
            .basic(&socket, &fixture.endpoints[..1])
            .await
            .unwrap();
        let error = engine
            .advanced(&socket, answers[0].0, &answers[0].1)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Filtering response was rejected or invalid"));
        assert!(report.advanced_capable);
        assert_eq!(report.mapping, Behavior::AddressAndPortDependent);
        assert_eq!(report.filtering, Behavior::Unknown);
        assert!(report.traditional_type.is_none());
        let last = report.observations.last().unwrap();
        match fault {
            fixture::FilteringFault::Rejected => assert!(last.response_received),
            fixture::FilteringFault::InvalidPackets => {
                assert_eq!(last.discarded_packets, MAX_DISCARDED_PACKETS)
            }
        }
    }
}
#[tokio::test]
async fn wrong_sources_transactions_and_truncations_are_discarded() {
    let fixture = fixture::start(fixture::Config {
        malformed_first: true,
        ..Default::default()
    })
    .await;
    let guard = LocalGuard {
        checks: Cell::new(0),
        fail_after: usize::MAX,
    };
    let socket = guard.bind().unwrap();
    let mut report = report();
    let emit = |_: ToolEvent| Ok(());
    let retries = [Duration::from_millis(50)];
    let mut engine = Engine {
        guard: &guard,
        report: &mut report,
        emit: &emit,
        observed: HashMap::new(),
        retries: &retries,
        next_transaction: tokio::time::Instant::now(),
    };
    engine
        .basic(&socket, &fixture.endpoints[..1])
        .await
        .unwrap();
    assert!(report
        .observations
        .iter()
        .all(|o| o.response_received && o.discarded_packets >= 3));
}
#[tokio::test]
async fn interface_recheck_failure_stops_without_another_send() {
    let fixture = fixture::start(Default::default()).await;
    let guard = LocalGuard {
        checks: Cell::new(0),
        fail_after: 2,
    };
    let socket = guard.bind().unwrap();
    let mut report = report();
    let emit = |_: ToolEvent| Ok(());
    let retries = [Duration::from_millis(50)];
    let mut engine = Engine {
        guard: &guard,
        report: &mut report,
        emit: &emit,
        observed: HashMap::new(),
        retries: &retries,
        next_transaction: tokio::time::Instant::now(),
    };
    assert!(engine.basic(&socket, &fixture.endpoints).await.is_err());
    assert!(!report.physical_path.as_ref().unwrap().binding_verified);
    assert!(!report.physical_path.as_ref().unwrap().route_verified);
    assert_eq!(report.observations.len(), 1);
    assert_eq!(
        fixture
            .contacts
            .lock()
            .unwrap()
            .values()
            .map(|s| s.len())
            .sum::<usize>(),
        1
    );
}
#[tokio::test]
async fn cancellation_drops_inflight_retransmissions() {
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = receiver.local_addr().unwrap();
    let guard = LocalGuard {
        checks: Cell::new(0),
        fail_after: usize::MAX,
    };
    let socket = guard.bind().unwrap();
    let mut report = report();
    let emit = |_: ToolEvent| Ok(());
    let retries = [Duration::from_millis(50), Duration::from_millis(100)];
    let mut engine = Engine {
        guard: &guard,
        report: &mut report,
        emit: &emit,
        observed: HashMap::new(),
        retries: &retries,
        next_transaction: tokio::time::Instant::now(),
    };
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        engine.exchange(&socket, target, target, 0, "cancelled")
    )
    .await
    .is_err());
    let mut packet = [0; 100];
    receiver.recv_from(&mut packet).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(110), receiver.recv_from(&mut packet))
            .await
            .is_err()
    );
}
#[test]
fn binding_codec_ipv4_ipv6_and_rejects_lengths_identity_origin() {
    let id = [7; 12];
    let source = "127.0.0.1:3478".parse().unwrap();
    let mapped = "198.51.100.8:54321".parse().unwrap();
    let packet = fixture::response(
        id,
        mapped,
        Some(source),
        Some("127.0.0.2:3479".parse().unwrap()),
    );
    assert_eq!(
        stun::decode(&packet, id, source, source).unwrap().mapped,
        mapped
    );
    assert!(stun::decode(&packet, [8; 12], source, source).is_err());
    assert!(stun::decode(&packet[..packet.len() - 1], id, source, source).is_err());
    assert!(stun::decode(
        &packet,
        id,
        source,
        SocketAddr::from((Ipv4Addr::LOCALHOST, 99))
    )
    .is_err());
    let mut trailing = packet.clone();
    trailing.push(0);
    assert!(stun::decode(&trailing, id, source, source).is_err());
    let source = "[::1]:3478".parse().unwrap();
    let mapped = "[2001:db8::1234]:65530".parse().unwrap();
    let packet = fixture::response(id, mapped, Some(source), None);
    assert_eq!(
        stun::decode(&packet, id, source, source).unwrap().mapped,
        mapped
    );
}

/// This guard is test-only: veth is intentionally not eligible for production
/// PhysicalGuard. The harness tests real kernel translation/filtering in an
/// isolated namespace; physical classification has separate platform tests.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires scripts/test_nat_namespaces.py and isolated Linux namespaces"]
async fn network_namespace_matrix_case() {
    struct NamespaceGuard(Cell<u16>);
    impl Guard for NamespaceGuard {
        fn bind(&self) -> std::io::Result<UdpSocket> {
            let port = self.0.get();
            self.0.set(port + 1);
            let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::new(10, 77, 0, 2), port))?;
            socket.set_nonblocking(true)?;
            UdpSocket::from_std(socket)
        }
        async fn check(&self, target: SocketAddr) -> Result<()> {
            ensure!(
                [
                    "198.18.0.2".parse::<std::net::IpAddr>().unwrap(),
                    "198.18.0.3".parse().unwrap()
                ]
                .contains(&target.ip()),
                "Namespace fixture may contact only its isolated server"
            );
            Ok(())
        }
    }
    let expected = |key| {
        serde_json::from_value::<Behavior>(serde_json::Value::String(
            std::env::var(key).expect("run through namespace harness"),
        ))
        .unwrap()
    };
    let guard = NamespaceGuard(Cell::new(35000));
    let socket = guard.bind().unwrap();
    let mut report = report();
    let emit = |_: ToolEvent| Ok(());
    let retries = [
        Duration::from_millis(100),
        Duration::from_millis(200),
        Duration::from_millis(400),
    ];
    let mut engine = Engine {
        guard: &guard,
        report: &mut report,
        emit: &emit,
        observed: HashMap::new(),
        retries: &retries,
        next_transaction: tokio::time::Instant::now(),
    };
    let primary = "198.18.0.2:3478".parse().unwrap();
    let answers = engine.basic(&socket, &[primary]).await.unwrap();
    engine
        .advanced(&socket, primary, &answers[0].1)
        .await
        .unwrap();
    assert_eq!(report.mapping, expected("ASTRA_NAT_FIXTURE_MAPPING"));
    assert_eq!(report.filtering, expected("ASTRA_NAT_FIXTURE_FILTERING"));
    assert!(!report.unstable);
}
