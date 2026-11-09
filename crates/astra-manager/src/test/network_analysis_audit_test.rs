use super::tests::{context, count, dns_comparison, event};
use super::*;

fn connection(purpose: &str, address: &str, interface: Option<&str>) -> Event {
    Event::Connection {
        seq: 1,
        purpose: purpose.into(),
        host: "same.test".into(),
        address: Some(address.into()),
        interface: interface.map(str::to_owned),
        address_source: "socket_peer".into(),
        family: None,
    }
}
fn handshake(name: &str, received: u32) -> Event {
    Event::Tls {
        seq: 1,
        detail: Box::new(TlsDetail::Handshake {
            server_name: name.into(),
            verified: received > 0,
            system_chain_verified: (received > 0).then_some(true),
            verification_attempted: received > 0,
            verification_error: None,
            handshake_error: None,
            protocol: None,
            cipher: None,
            alpn: None,
            handshake_ms: 1.,
            received_certificates: received,
            retained_certificates: received,
            retained_der_bytes: received * 100,
            truncated: false,
            bundled_comparison: None,
        }),
    }
}
fn certificate(fingerprint: &str) -> Event {
    Event::Tls {
        seq: 1,
        detail: Box::new(TlsDetail::Certificate {
            index: 0,
            sha256: fingerprint.into(),
            subject: None,
            issuer: None,
            not_before: None,
            not_after: None,
            subject_alt_names: vec![],
            der_bytes: 100,
            metadata_truncated: false,
            parse_error: None,
        }),
    }
}
#[test]
fn tls_connection_and_handshake_boundaries_never_inherit_auxiliary_certificates() {
    for target_certificate in [false, true] {
        let mut c = Collector {
            comparison: true,
            ..Default::default()
        };
        for round in [1, 2] {
            for id in ["a", "b"] {
                for detail in [
                    Event::Context(Box::new(context(None))),
                    connection("dns_service", "192.0.2.53:853", None),
                    handshake("resolver.test", 1),
                    certificate(id),
                    connection("business_target", "192.0.2.1:443", None),
                    handshake("same.test", u32::from(target_certificate)),
                ] {
                    c.observe(&event(id, id, round, 1000 * u64::from(round), detail));
                }
                if target_certificate {
                    c.observe(&event(
                        id,
                        id,
                        round,
                        1001 * u64::from(round),
                        certificate(id),
                    ));
                }
                c.projects.get_mut(&(id.into(), round)).unwrap().complete = true;
            }
        }
        assert_eq!(
            count(c.finish(false), FindingKind::CertificateDifference),
            usize::from(target_certificate)
        );
    }
    let mut c = Collector::default();
    for detail in [
        connection("business_target", "192.0.2.1:443", None),
        handshake("same.test", 1),
        certificate("old"),
        handshake("same.test", 0),
    ] {
        c.observe(&event("p", "p", 1, 1000, detail));
    }
    assert!(c.projects[&("p".into(), 1)].tls[&1].fingerprint.is_none());
}
#[test]
fn capped_references_preserve_late_and_invalid_times() {
    for last in ["40000", "invalid"] {
        let mut c = dns_comparison();
        let refs = &mut c
            .projects
            .get_mut(&("a".into(), 2))
            .unwrap()
            .dns
            .get_mut(&1)
            .unwrap()
            .evidence;
        for time in 3001..3020 {
            let mut e = refs[0].clone();
            e.sampled_at_ms = time.to_string();
            push_ref(refs, e);
        }
        let mut e = refs[0].clone();
        e.sampled_at_ms = last.into();
        push_ref(refs, e);
        assert_eq!(refs.len(), 8);
        assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
    }
}
#[test]
fn dns_comparison_rejects_resolver_changes_between_rounds() {
    let mut c = dns_comparison();
    c.projects
        .get_mut(&("a".into(), 2))
        .unwrap()
        .dns
        .get_mut(&1)
        .unwrap()
        .server = "192.0.2.99:53".into();
    assert_eq!(count(c.finish(false), FindingKind::DnsDifference), 0);
}
#[test]
fn project_status_and_retained_dns_evidence_are_bounded_before_report_serialization() {
    let mut c = Collector::default();
    for n in 0..100 {
        c.observe(&Event::ProjectStatus {
            project_id: n.to_string(),
            path_id: "p".into(),
            round: 1,
            state: "queued".into(),
            message: "fixture".into(),
        });
    }
    assert_eq!(c.projects.len(), 64);
    assert!(c.finish(false).truncated);
    let mut c = Collector::default();
    for n in 0..60 {
        c.observe(&event(
            "p",
            "p",
            1,
            n,
            Event::Dns {
                seq: n as u32,
                detail: Box::new(DnsDetail::Records {
                    section: "answer".into(),
                    records: vec![record(&"x".repeat(50_000))],
                }),
            },
        ));
    }
    assert!(c.projects[&("p".into(), 1)].dns.len() < 50);
    let report = c.finish(false);
    assert!(report.truncated && report.incomplete);
}
fn record(text: &str) -> astra_model::nettool::DnsRecord {
    serde_json::from_value(serde_json::json!({"name":"same.test.","record_type":"TXT","class":"IN","ttl":60,"text":text,"text_truncated":false})).unwrap()
}
#[test]
fn dns_answer_cap_is_explicit_and_existing_sequence_is_updated_at_sample_limit() {
    let mut c = Collector::default();
    for seq in 0..64 {
        c.observe(&event(
            "p",
            "p",
            1,
            1000,
            Event::Dns {
                seq,
                detail: Box::new(DnsDetail::Records {
                    section: "answer".into(),
                    records: vec![record("first")],
                }),
            },
        ));
    }
    c.observe(&event(
        "p",
        "p",
        1,
        40000,
        Event::Dns {
            seq: 0,
            detail: Box::new(DnsDetail::Records {
                section: "answer".into(),
                records: (0..257).map(|n| record(&n.to_string())).collect(),
            }),
        },
    ));
    let sample = &c.projects[&("p".into(), 1)].dns[&0];
    assert_eq!(sample.records.len(), 256);
    assert!(sample.truncated);
    assert!(sample.evidence.iter().any(|e| e.sampled_at_ms == "40000"));
}
#[test]
fn only_executed_known_dns_transports_and_explicit_interface_conflicts_trigger_risks() {
    for (method, resolver, actual, expected_plain, expected_mismatch) in [
        ("system", None, None, 0, 0),
        (
            "explicit_resolver",
            Some("[resolver details omitted]"),
            Some("eth0"),
            0,
            0,
        ),
        (
            "explicit_resolver",
            Some("udp://192.0.2.53:53"),
            Some("eth1"),
            1,
            1,
        ),
    ] {
        let mut c = Collector::default();
        c.observe(&event(
            "p",
            "p",
            1,
            1000,
            Event::Context(Box::new(context(Some("eth0")))),
        ));
        c.observe(&event(
            "p",
            "p",
            1,
            1001,
            Event::Resolution {
                seq: 1,
                purpose: "business_target".into(),
                host: "same.test".into(),
                method: method.into(),
                resolver: resolver.map(str::to_owned),
                addresses: vec!["192.0.2.1".into()],
                interface: actual.map(str::to_owned),
            },
        ));
        let report = c.finish(false);
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|f| f.kind == FindingKind::PlaintextDns)
                .count(),
            expected_plain
        );
        assert_eq!(
            report
                .findings
                .iter()
                .filter(|f| f.kind == FindingKind::ResolutionPathMismatch)
                .count(),
            expected_mismatch
        );
    }
}
#[test]
fn topology_distinguishes_address_input_hidden_resolution_and_real_resolver_exchange() {
    for (method, expected) in [
        ("literal", EvidenceLevel::Configuration),
        ("explicit_bootstrap", EvidenceLevel::Configuration),
        ("remote_proxy", EvidenceLevel::Unknown),
        ("system", EvidenceLevel::Unknown),
        ("explicit_resolver", EvidenceLevel::Measured),
    ] {
        let mut c = Collector::default();
        c.observe(&event(
            "p",
            "p",
            1,
            1000,
            Event::Resolution {
                seq: 1,
                purpose: "business_target".into(),
                host: "same.test".into(),
                method: method.into(),
                resolver: Some("192.0.2.53:53".into()),
                addresses: vec!["192.0.2.1".into()],
                interface: None,
            },
        ));
        assert_eq!(c.finish(false).topology[0].edges[0].level, expected);
    }
    let mut c = Collector::default();
    for suffix in ["a", "b"] {
        let mut detail = connection("business_target", "192.0.2.1:443", None);
        if let Event::Connection { host, .. } = &mut detail {
            *host = format!("{}{suffix}", "x".repeat(700));
        }
        c.observe(&event("p", "p", 1, 1000, detail));
    }
    assert_eq!(
        c.finish(false).topology[0]
            .nodes
            .iter()
            .filter(|n| n.role == "target")
            .count(),
        2
    );
}
#[test]
fn ipv4_ipv6_egress_comparison_uses_path_at_observation_not_later_connection() {
    for connect_first in [false, true] {
        let mut c = Collector::default();
        for (id, address, iface) in [("a", "192.0.2.1", "eth0"), ("b", "2001:db8::1", "eth1")] {
            c.observe(&event(
                id,
                id,
                1,
                1000,
                Event::Context(Box::new(context(None))),
            ));
            let conn = event(
                id,
                id,
                1,
                1001,
                connection("business_target", "192.0.2.10:443", Some(iface)),
            );
            if connect_first {
                c.observe(&conn);
            }
            c.observe(&event(
                id,
                id,
                1,
                1002,
                Event::PublicIp {
                    seq: 1,
                    address: address.into(),
                    source: "https".into(),
                    service: "fixture".into(),
                },
            ));
            if !connect_first {
                c.observe(&conn);
            }
        }
        assert_eq!(
            count(c.finish(false), FindingKind::Ipv4Ipv6EgressDifference),
            usize::from(connect_first)
        );
    }
}

#[test]
fn cross_project_comparison_work_is_bounded_even_without_matching_findings() {
    let mut c = Collector {
        comparison: true,
        ..Default::default()
    };
    for round in [1, 2] {
        for id in 0..32 {
            let mut p = Project {
                context: Some(context(None)),
                complete: true,
                ..Default::default()
            };
            for seq in 0..64 {
                p.dns.insert(
                    seq,
                    DnsSample {
                        question: "same.test/A".into(),
                        server: format!("192.0.2.{id}:53"),
                        rcode: "NOERROR".into(),
                        records: ["same-answer".into()].into(),
                        expected: 1,
                        retained: 1,
                        valid: true,
                        ..Default::default()
                    },
                );
            }
            c.projects.insert((id.to_string(), round), p);
        }
    }
    let report = c.finish(false);
    assert!(report.incomplete && report.truncated);
    assert!(report.findings.is_empty());
}
