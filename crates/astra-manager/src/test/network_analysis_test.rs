use super::*;
pub(super) fn event(project: &str, path: &str, round: u8, time: u64, detail: Event) -> Event {
    Event::Project {
        project: ProjectContext {
            project_id: project.into(),
            path_id: path.into(),
            round,
            index: 1,
            sampled_at_ms: time.to_string(),
            comparison_requested: true,
        },
        detail: Box::new(detail),
    }
}
#[test]
fn no_certificate_evidence_or_network_timeout_is_not_a_tls_verification_failure() {
    let mut collector = Collector::default();
    collector.observe(&event(
        "tls",
        "p",
        1,
        1000,
        Event::Tls {
            seq: 1,
            detail: Box::new(TlsDetail::Handshake {
                server_name: "example.test".into(),
                verified: false,
                system_chain_verified: None,
                verification_attempted: false,
                verification_error: None,
                handshake_error: Some("timeout".into()),
                protocol: None,
                cipher: None,
                alpn: None,
                handshake_ms: 100.,
                received_certificates: 0,
                retained_certificates: 0,
                retained_der_bytes: 0,
                truncated: false,
                bundled_comparison: None,
            }),
        },
    ));
    let report = collector.finish(true);
    assert!(report.findings.is_empty());
    assert!(report.incomplete);
}
#[test]
fn explicit_verifier_error_is_reported_without_claiming_interception() {
    let mut collector = Collector::default();
    collector.observe(&event(
        "tls",
        "p",
        1,
        1000,
        Event::Tls {
            seq: 1,
            detail: Box::new(TlsDetail::Handshake {
                server_name: "example.test".into(),
                verified: false,
                system_chain_verified: Some(false),
                verification_attempted: true,
                verification_error: Some("UnknownIssuer".into()),
                handshake_error: Some("certificate validation failed".into()),
                protocol: None,
                cipher: None,
                alpn: None,
                handshake_ms: 10.,
                received_certificates: 1,
                retained_certificates: 1,
                retained_der_bytes: 100,
                truncated: false,
                bundled_comparison: None,
            }),
        },
    ));
    let report = collector.finish(false);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].kind, FindingKind::TlsVerificationFailed);
    assert_eq!(report.findings[0].severity, Severity::NeedsVerification);
}
#[test]
fn topology_does_not_invent_routers_or_connected_adjacent_hops() {
    let mut collector = Collector::default();
    collector.observe(&event(
        "trace",
        "physical",
        1,
        1000,
        Event::Trace {
            seq: 1,
            sample: astra_model::network_path::TraceSample {
                hop: 3,
                probe: 1,
                address: None,
                rtt_ms: None,
                outcome: astra_model::network_path::PathOutcome::NoResponse,
                icmp_type: None,
                icmp_code: None,
                message: None,
            },
        },
    ));
    let report = collector.finish(false);
    assert!(report.findings.is_empty());
    assert_eq!(report.topology[0].edges[0].level, EvidenceLevel::Unknown);
    assert_eq!(report.topology[0].edges[0].from, "agent");
}

pub(super) fn context(interface: Option<&str>) -> DiagnosticContext {
    serde_json::from_value(serde_json::json!({
        "sampled_at_ms":"1000","running_core":null,"config_digest":"fixture-config","config_observation":"matched_running",
        "actual_egress":if let Some(name)=interface {serde_json::json!({"kind":"interface","name":name})} else {serde_json::json!({"kind":"system"})},
        "egress_source":"requested","config_used":false,"selection_chain":[],"effective_interface":interface,"interface_source":null,
        "probe_binary":{"name":"astra-proxy","version":"test","compiled_at":"test","target":"test","profile":"test","rustc":"test","git_revision":null,"capability_process_id":1,"executable":null,"capabilities":[]},"legacy_details":false
    })).unwrap()
}
fn dns_sample(
    question: &str,
    server: &str,
    address: &str,
    project: &str,
    round: u8,
    time: u64,
) -> DnsSample {
    DnsSample {
        question: question.into(),
        server: server.into(),
        rcode: "NOERROR".into(),
        records: [address.to_owned()].into(),
        expected: 1,
        retained: 1,
        valid: true,
        truncated: false,
        evidence: vec![EvidenceRef {
            project_id: project.into(),
            path_id: project.into(),
            round,
            event_index: 1,
            sampled_at_ms: time.to_string(),
            source: "dns_response".into(),
        }],
    }
}
pub(super) fn dns_comparison() -> Collector {
    let mut c = Collector {
        comparison: true,
        ..Default::default()
    };
    for round in [1, 2] {
        for (id, server, answer) in [
            ("a", "127.0.0.1:53", "192.0.2.1"),
            ("b", "127.0.0.2:53", "192.0.2.2"),
        ] {
            c.projects.insert(
                (id.into(), round),
                Project {
                    context: Some(context(None)),
                    complete: true,
                    dns: [(
                        1,
                        dns_sample(
                            "same.test/A",
                            server,
                            answer,
                            id,
                            round,
                            1000 + 1000 * round as u64,
                        ),
                    )]
                    .into(),
                    ..Default::default()
                },
            );
        }
    }
    c
}
pub(super) fn count(report: AnalysisReport, kind: FindingKind) -> usize {
    report.findings.iter().filter(|f| f.kind == kind).count()
}
#[test]
fn dns_difference_requires_explicit_two_round_consent_same_question_and_window() {
    assert_eq!(
        count(dns_comparison().finish(false), FindingKind::DnsDifference),
        1
    );
    let mut c = dns_comparison();
    c.comparison = false;
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
    let mut c = dns_comparison();
    c.projects.remove(&("b".into(), 2));
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
    let mut c = dns_comparison();
    c.projects
        .get_mut(&("b".into(), 2))
        .unwrap()
        .dns
        .get_mut(&1)
        .unwrap()
        .records = ["192.0.2.1".into()].into();
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
    let mut c = dns_comparison();
    c.projects
        .get_mut(&("b".into(), 2))
        .unwrap()
        .dns
        .get_mut(&1)
        .unwrap()
        .question = "other.test/A".into();
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
    let mut c = dns_comparison();
    c.projects
        .get_mut(&("b".into(), 2))
        .unwrap()
        .dns
        .get_mut(&1)
        .unwrap()
        .evidence[0]
        .sampled_at_ms = "40000".into();
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
}
#[test]
fn incomplete_or_truncated_evidence_and_path_changes_do_not_create_dns_difference() {
    let mut c = dns_comparison();
    c.projects
        .get_mut(&("a".into(), 2))
        .unwrap()
        .dns
        .get_mut(&1)
        .unwrap()
        .truncated = true;
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
    let mut c = dns_comparison();
    c.projects
        .get_mut(&("a".into(), 2))
        .unwrap()
        .dns
        .get_mut(&1)
        .unwrap()
        .retained = 0;
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
    let mut c = dns_comparison();
    c.projects
        .get_mut(&("a".into(), 2))
        .unwrap()
        .context
        .as_mut()
        .unwrap()
        .config_digest = Some("reload".into());
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
    assert_eq!(
        count(dns_comparison().finish(true), FindingKind::DnsDifference),
        0
    );
}
#[test]
fn certificate_difference_requires_actual_same_ip_and_two_rounds() {
    let fixture = || {
        let mut c = dns_comparison();
        for ((id, round), project) in &mut c.projects {
            let e = project.dns[&1].evidence.clone();
            project.dns.clear();
            project.tls.insert(
                1,
                TlsSample {
                    name: "same.test".into(),
                    address: Some("192.0.2.1".into()),
                    fingerprint: Some(id.clone()),
                    verified: true,
                    truncated: false,
                    accept_leaf: true,
                    evidence: e,
                },
            );
            assert!([1, 2].contains(round));
        }
        c
    };
    assert_eq!(
        count(fixture().finish(false), FindingKind::CertificateDifference),
        1
    );
    let mut c = fixture();
    c.projects
        .get_mut(&("b".into(), 1))
        .unwrap()
        .tls
        .get_mut(&1)
        .unwrap()
        .address = None;
    assert_eq!(
        count(c.finish(false), FindingKind::CertificateDifference),
        0
    );
    let mut c = fixture();
    c.projects
        .get_mut(&("b".into(), 2))
        .unwrap()
        .tls
        .get_mut(&1)
        .unwrap()
        .address = Some("192.0.2.2".into());
    assert_eq!(
        count(c.finish(false), FindingKind::CertificateDifference),
        0
    );
}
fn ip_comparison(left: &str, right: &str, source: &str, round: u8) -> Collector {
    let mut c = Collector::default();
    for (id, addr, source, round) in [("https", left, "https", 1), ("other", right, source, round)]
    {
        c.observe(&event(
            id,
            "same",
            round,
            1000,
            Event::Context(Box::new(context(None))),
        ));
        c.observe(&event(
            id,
            "same",
            round,
            1001,
            Event::PublicIp {
                seq: 1,
                address: addr.into(),
                source: source.into(),
                service: "fixture".into(),
            },
        ));
    }
    c
}
#[test]
fn https_stun_difference_never_crosses_family_round_or_pinned_identity() {
    assert_eq!(
        count(
            ip_comparison("192.0.2.1", "192.0.2.2", "stun", 1).finish(false),
            FindingKind::HttpsStunEgressDifference
        ),
        1
    );
    assert_eq!(
        count(
            ip_comparison("192.0.2.1", "2001:db8::1", "stun", 1).finish(false),
            FindingKind::HttpsStunEgressDifference
        ),
        0
    );
    assert_eq!(
        count(
            ip_comparison("192.0.2.1", "192.0.2.2", "stun", 2).finish(false),
            FindingKind::HttpsStunEgressDifference
        ),
        0
    );
    let mut c = ip_comparison("192.0.2.1", "192.0.2.2", "stun", 1);
    c.projects
        .get_mut(&("other".into(), 1))
        .unwrap()
        .context
        .as_mut()
        .unwrap()
        .config_digest = Some("reload".into());
    assert_eq!(
        count(c.finish(false), FindingKind::HttpsStunEgressDifference),
        0
    );
    assert_eq!(
        count(
            ip_comparison("192.0.2.1", "2001:db8::1", "https", 1).finish(false),
            FindingKind::Ipv4Ipv6EgressDifference
        ),
        0
    );
}
#[test]
fn proxy_plaintext_finding_requires_current_configuration_and_successful_protocol() {
    let fixture = |matched, connected| {
        let mut c = Collector::default();
        c.observe(&event(
            "p",
            "p",
            1,
            1000,
            Event::Policy(PathPolicy {
                protocol: "socks5".into(),
                encrypted: Some(false),
                certificate_verification: None,
                matched_running: matched,
                leaf_tag: Some("test".into()),
            }),
        ));
        if connected {
            c.observe(&event(
                "p",
                "p",
                1,
                1001,
                Event::Connection {
                    seq: 1,
                    purpose: "business_target".into(),
                    host: "test".into(),
                    address: None,
                    interface: None,
                    address_source: "remote_dns_unknown".into(),
                    family: None,
                },
            ));
        }
        c.finish(false)
    };
    assert_eq!(
        count(fixture(true, true), FindingKind::PlaintextProxySegment),
        1
    );
    assert_eq!(
        count(fixture(false, true), FindingKind::PlaintextProxySegment),
        0
    );
    assert_eq!(
        count(fixture(true, false), FindingKind::PlaintextProxySegment),
        0
    );
}
#[test]
fn topology_and_analysis_fit_one_report_page_with_explicit_truncation() {
    let mut c = Collector::default();
    for n in 0..300 {
        c.observe(&event(
            "p",
            "p",
            1,
            n,
            Event::Resolution {
                seq: 1,
                purpose: "business_target".into(),
                host: "x".repeat(1024),
                method: "explicit_resolver".into(),
                resolver: Some(format!("192.0.2.{}", n)),
                addresses: vec!["192.0.2.1:80".into()],
                interface: None,
            },
        ));
    }
    let report = c.finish(false);
    assert!(report.truncated);
    assert!(report.incomplete);
    assert!(serde_json::to_vec(&report).unwrap().len() <= 52 * 1024);
}

#[test]
fn signature_failure_is_not_a_root_store_difference() {
    for (chain, bundled, expected) in [(true, true, 0), (true, false, 1), (false, true, 1)] {
        let mut c = Collector::default();
        c.observe(&event(
            "tls",
            "p",
            1,
            1000,
            Event::Tls {
                seq: 1,
                detail: Box::new(TlsDetail::Handshake {
                    server_name: "same.test".into(),
                    verified: false,
                    system_chain_verified: Some(chain),
                    verification_attempted: true,
                    verification_error: Some("Handshake signature: invalid".into()),
                    handshake_error: Some("failed".into()),
                    protocol: None,
                    cipher: None,
                    alpn: None,
                    handshake_ms: 1.,
                    received_certificates: 1,
                    retained_certificates: 1,
                    retained_der_bytes: 100,
                    truncated: false,
                    bundled_comparison: Some(astra_model::network_tls::TrustComparison {
                        source: "bundled".into(),
                        verified: bundled,
                        error: None,
                        offline: true,
                    }),
                }),
            },
        ));
        assert_eq!(
            count(c.finish(false), FindingKind::TrustStoreDifference),
            expected
        );
    }
}
#[test]
fn encrypted_auxiliary_dns_is_never_classified_as_plaintext() {
    for resolver in [
        "dot://127.0.0.1:853",
        "tls://127.0.0.1:853",
        "https://resolver.test:443/[path omitted]",
    ] {
        let mut c = Collector::default();
        c.observe(&event(
            "tls",
            "p",
            1,
            1000,
            Event::Resolution {
                seq: 1,
                purpose: "business_target".into(),
                host: "same.test".into(),
                method: "explicit_resolver".into(),
                resolver: Some(resolver.into()),
                addresses: vec!["192.0.2.1:443".into()],
                interface: None,
            },
        ));
        assert_eq!(count(c.finish(false), FindingKind::PlaintextDns), 0);
    }
}
