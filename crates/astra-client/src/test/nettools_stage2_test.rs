use super::*;
use serde_json::json;

fn view() -> View {
    let mut view = View::new(
        Path::new("unused-stage2.sock"),
        "fixture".into(),
        &["DIRECT".into(), "proxy".into(), "fourth".into()],
    );
    view.next_refresh = std::time::Instant::now() + std::time::Duration::from_secs(60);
    view.context.network = Some(
        json!({"agent_id":"remote-agent","truncated":false,"interfaces":[
            {"id":"physical-7","name":"Remote Ethernet","kind":"physical","connected":true,"addresses":[{"address":"192.0.2.7"},{"address":"2001:db8::7"}]},
            {"id":"tun-9","name":"TUN","kind":"virtual","connected":true,"addresses":[{"address":"192.0.2.9"}]}
        ]}),
    );
    view
}
#[test]
fn every_tool_has_editable_fields_and_a_specific_request() {
    let mut v = view();
    for kind in Kind::ALL {
        v.set_kind(kind);
        let fields = v.fields();
        assert!(fields.contains(&Field::Kind) && fields.contains(&Field::Target));
        for field in fields {
            assert!(!field.label().is_empty());
            let _ = v.field_value(field);
        }
        if kind == Kind::Nat {
            assert!(v.request().is_err());
            v.select_physical("physical-7".into());
        }
        if matches!(kind, Kind::FakeIp | Kind::Report) {
            assert!(v.request().is_err());
            continue;
        }
        assert!(v.request().is_ok(), "{:?}: {:?}", kind, v.request());
    }
    v.set_kind(Kind::Tls);
    v.target = "[2001:db8::1]:8443".into();
    v.server_name = "service.example".into();
    v.compare_trust = true;
    assert!(
        matches!(v.request().unwrap().probe,Probe::Tls {host,port:8443,server_name:Some(name),compare_bundled:true} if host=="2001:db8::1"&&name=="service.example")
    );
    v.set_kind(Kind::Trace);
    v.set_text(Field::Hops, "12".into()).unwrap();
    v.set_text(Field::Samples, "2".into()).unwrap();
    assert!(matches!(
        v.request().unwrap().probe,
        Probe::Trace {
            max_hops: 12,
            probes_per_hop: 2,
            ..
        }
    ));
    assert!(v.set_text(Field::Samples, "4".into()).is_err());
}
#[test]
fn batches_keep_paths_separate_and_do_not_copy_nat_or_urls_into_summaries() {
    let mut v = view();
    v.set_kind(Kind::Dns);
    v.target = "example.test".into();
    v.server = "https://dns.test/secret?token=DNS_SECRET#bootstrap=192.0.2.53".into();
    v.resolver = "https://resolver.test/private?token=RESOLVER_SECRET#bootstrap=192.0.2.54".into();
    v.count = 4;
    v.add_project().unwrap();
    v.set_kind(Kind::Http);
    v.target = "https://service.test/private?token=HTTP_SECRET".into();
    v.add_project().unwrap();
    v.select_egress(1);
    v.copy_plan_path().unwrap();
    assert_eq!(v.plan.projects.len(), 4);
    v.select_egress(2);
    v.copy_plan_path().unwrap();
    assert_eq!(v.plan.projects.len(), 6);
    v.select_egress(3);
    assert!(v.copy_plan_path().is_err());
    assert_eq!(v.plan.projects.len(), 6);
    let lines = v.plan_lines().join("\n");
    for secret in [
        "DNS_SECRET",
        "RESOLVER_SECRET",
        "HTTP_SECRET",
        "/private",
        "/secret",
    ] {
        assert!(!lines.contains(secret));
    }
    let paths = v
        .plan
        .projects
        .iter()
        .map(|p| &p.path_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(paths.len(), 3);
    v.plan.comparison = true;
    let batch = v.batch_request().unwrap();
    assert!(batch.comparison);
    assert!(batch
        .projects
        .iter()
        .filter(|p| matches!(p.request.probe, Probe::Dns { .. }))
        .all(|p| p.request.count == 1));
    assert_eq!(
        v.plan.projects[0].request.count, 4,
        "normal draft sampling must not be mutated by comparison"
    );
}
#[test]
fn editing_preserves_all_options_and_requires_nat_reselection() {
    let mut v = view();
    v.set_kind(Kind::Tls);
    v.target = "192.0.2.8:9443".into();
    v.server_name = "tls.test".into();
    v.compare_trust = true;
    v.resolver = "udp://192.0.2.53".into();
    v.family = Some(astra_model::nettool::AddressFamily::Ipv6);
    v.count = 3;
    v.select_egress(2);
    v.add_project().unwrap();
    let original = v.plan.projects[0].request.clone();
    v.set_kind(Kind::Dns);
    v.edit_project(0).unwrap();
    assert_eq!(v.kind, Kind::Tls);
    assert!(v.plan.projects.is_empty());
    v.add_project().unwrap();
    assert_eq!(
        serde_json::to_value(original).unwrap(),
        serde_json::to_value(&v.plan.projects[0].request).unwrap()
    );
    v.plan.projects.clear();
    v.set_kind(Kind::Nat);
    assert_eq!(v.physical_choices().len(), 1);
    v.select_physical("tun-9".into());
    assert!(v.nat_interface.is_none());
    v.select_physical("physical-7".into());
    v.add_project().unwrap();
    assert!(v.batch_request().is_ok());
    assert!(v.copy_plan_path().is_err());
    v.nat_interface = None;
    assert!(v.batch_request().is_err());
    v.select_physical("physical-7".into());
    assert!(v.batch_request().is_ok());
    v.context.network.as_mut().unwrap()["agent_id"] = json!("different-agent");
    assert!(v.batch_request().is_err());
    v.edit_project(0).unwrap();
    assert!(
        v.nat_interface.is_none(),
        "editing a saved NAT project cannot auto-select its adapter"
    );
}
#[test]
fn analysis_findings_and_three_topologies_preserve_backend_provenance() {
    use astra_model::network_analysis::*;
    let evidence = EvidenceRef {
        project_id: "dns-1".into(),
        path_id: "path-1".into(),
        round: 2,
        event_index: 7,
        sampled_at_ms: "1000".into(),
        source: "measured dns".into(),
    };
    let report = AnalysisReport {
        rules_version: 1,
        comparison_requested: true,
        comparison_status: "compared fixture".into(),
        findings: vec![Finding {
            kind: FindingKind::DnsDifference,
            confidence: Confidence::Medium,
            severity: Severity::NeedsVerification,
            title: "BACKEND TITLE".into(),
            explanation: "BACKEND EXPLANATION".into(),
            alternatives: vec!["BACKEND ALTERNATIVE".into()],
            recommendation: "BACKEND RECOMMENDATION".into(),
            evidence: vec![evidence.clone()],
        }],
        topology: vec![TopologyPath {
            path_id: "path-1".into(),
            nodes: vec![
                TopologyNode {
                    id: "a".into(),
                    label: "resolver".into(),
                    role: "dns service".into(),
                },
                TopologyNode {
                    id: "b".into(),
                    label: "answer".into(),
                    role: "business".into(),
                },
            ],
            edges: vec![TopologyEdge {
                from: "a".into(),
                to: "b".into(),
                section: PathSection::Dns,
                level: EvidenceLevel::Measured,
                description: "measured DNS only".into(),
                evidence: vec![evidence.clone()],
            }],
            attachments: vec![TopologyAttachment {
                kind: "nat".into(),
                label: "NAT attachment".into(),
                evidence: vec![evidence],
            }],
            truncated: false,
        }],
        visibility: vec![ObserverVisibility {
            location: "resolver observer".into(),
            information: "query names".into(),
        }],
        incomplete: true,
        truncated: true,
        notes: vec!["BACKEND NOTE".into()],
    };
    let mut v = view();
    v.details
        .push(serde_json::to_value(Event::Analysis(Box::new(report))).unwrap());
    v.section = Section::Findings;
    v.show_advanced = true;
    let result = v.output().join("\n");
    for part in [
        "BACKEND TITLE",
        "BACKEND EXPLANATION",
        "BACKEND ALTERNATIVE",
        "BACKEND RECOMMENDATION",
        "BACKEND NOTE",
        "resolver observer",
        "query names",
        "待核实",
        "置信度中",
        "dns-1",
        "事件 7",
        "来源 measured dns",
        "已截断",
    ] {
        assert!(result.contains(part), "missing {part}: {result}");
    }
    v.section = Section::Paths;
    let paths = v.output().join("\n");
    for part in [
        "DNS 解析视图",
        "连接分段视图",
        "逐跳测量视图",
        "实测",
        "旁注（不计作逐跳设备）",
        "NAT attachment",
    ] {
        assert!(paths.contains(part), "missing {part}");
    }
}
#[test]
fn project_wrapped_dns_and_tls_evidence_is_readable_and_der_stays_export_only() {
    let mut v = view();
    v.show_advanced = true;
    v.details.push(json!({"event":"project","project":{"project_id":"dns-1","path_id":"path-1","round":2,"index":3,"sampled_at_ms":"7"},"detail":{"event":"dns","seq":1,"detail":{"kind":"records","section":"answer","records":[{"name":"dns.test","class":"IN","record_type":"TXT","ttl":42,"text":format!("{}TAIL", "x".repeat(8192)),"text_truncated":false}]}}}));
    v.details.push(
        serde_json::to_value(Event::Tls {
            seq: 1,
            detail: Box::new(astra_model::network_tls::TlsDetail::CertificateDer {
                index: 0,
                offset: 0,
                total_bytes: 8,
                base64: "DER_CONTENT_SECRET".into(),
            }),
        })
        .unwrap(),
    );
    v.details.push(json!({"event":"http","seq":1,"detail":{"kind":"preview","hop":0,"offset":0,"text":"line one\nline two"}}));
    let lines = v.output();
    let text = lines.join("\n");
    assert!(
        text.contains("dns-1")
            && text.contains("第 2 轮")
            && text.contains("TTL=42")
            && text.contains("TAIL")
    );
    assert!(!text.contains("DER_CONTENT_SECRET"));
    assert!(lines.iter().any(|s| s == "line one") && lines.iter().any(|s| s == "line two"));
    assert!(v.export_json().unwrap().contains("DER_CONTENT_SECRET"));
}
