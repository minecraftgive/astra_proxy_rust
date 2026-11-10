use super::*;
use serde_json::json;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}
fn view() -> View {
    let mut view = View::new(
        Path::new("unused-nettool.sock"),
        "proxy".into(),
        &["DIRECT".into(), "HK 01".into(), "Auto".into()],
    );
    view.egresses = vec![
        Egress::System,
        Egress::Interface("Wi-Fi".into()),
        Egress::Outbound("DIRECT".into()),
        Egress::Outbound("HK 01".into()),
    ];
    view.context.network = Some(json!({
        "agent_id": "agent-1",
        "truncated": false,
        "interfaces": [
            {"id": "Wi-Fi", "name": "Wi-Fi", "kind": "physical", "connected": true,
             "addresses": [{"address": "192.168.1.5"}]}
        ],
        "routes": [{"destination": "default", "interface_id": "Wi-Fi"}]
    }));
    view
}

#[tokio::test]
async fn form_builds_requests_for_each_tool() {
    let mut view = view();
    assert_eq!(view.kind, Kind::Icmp);
    let request = view.request().unwrap();
    assert_eq!(
        request.probe,
        Probe::Icmp {
            host: "www.gstatic.com".into()
        }
    );
    assert_eq!(request.count, 4);

    view.set_kind(Kind::Tcp);
    assert_eq!(view.target, "www.gstatic.com:443");
    view.target = "example.com:80,443,8080".into();
    assert_eq!(
        view.request().unwrap().probe,
        Probe::TcpPorts {
            host: "example.com".into(),
            ports: vec![80, 443, 8080]
        }
    );
    view.target = "example.com:1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17".into();
    assert!(view.request().is_err(), "at most 16 ports");
    view.target = "example.com:443".into();
    view.resolver = "tcp://1.1.1.1:53".into();
    let request = view.request().unwrap();
    assert_eq!(request.resolver.as_deref(), Some("tcp://1.1.1.1:53"));

    view.set_kind(Kind::Dns);
    assert_eq!(view.target, "example.com");
    assert!(view.fields().contains(&Field::Record) && !view.fields().contains(&Field::Resolver));
    view.field = Field::Record;
    view.key(key(KeyCode::Right));
    let request = view.request().unwrap();
    assert!(matches!(&request.probe, Probe::Dns { record, .. } if record == "AAAA"));
    assert_eq!(request.resolver, None, "DNS uses its own server field");

    view.set_kind(Kind::Http);
    view.field = Field::HttpMethod;
    view.key(key(KeyCode::Char(' ')));
    assert!(matches!(
        view.request().unwrap().probe,
        Probe::Http { ref method, .. } if method == "HEAD"
    ));

    view.set_kind(Kind::PublicIp);
    assert!(matches!(
        view.request().unwrap().probe,
        Probe::HttpsIp { .. }
    ));
}

#[tokio::test]
async fn egress_choices_follow_what_each_tool_can_honour() {
    let mut view = view();
    view.set_kind(Kind::Trace);
    assert_eq!(
        view.egresses[view.egress],
        Egress::Interface("Wi-Fi".into())
    );
    let allowed: Vec<_> = view
        .allowed_egresses()
        .into_iter()
        .map(|(i, _)| view.egresses[i].clone())
        .collect();
    assert_eq!(
        allowed,
        vec![
            Egress::Interface("Wi-Fi".into()),
            Egress::Outbound("DIRECT".into())
        ]
    );
    let request = view.request().unwrap();
    assert!(matches!(
        request.probe,
        Probe::Trace {
            max_hops: 30,
            probes_per_hop: 3,
            ..
        }
    ));
    view.egress = 3;
    assert!(view.request().is_err(), "trace cannot look inside a proxy");

    view.set_kind(Kind::Icmp);
    view.egress = 3;
    view.set_kind(Kind::Tcp);
    view.set_kind(Kind::Icmp);
    assert_ne!(view.egresses[view.egress], Egress::Outbound("HK 01".into()));
    assert!(egress_allowed(Kind::Tcp, &Egress::Outbound("HK 01".into())));
}

#[tokio::test]
async fn nat_requires_a_fresh_physical_interface_choice() {
    let mut view = view();
    view.set_kind(Kind::Nat);
    assert!(!view.fields().contains(&Field::Egress));
    assert!(view.request().is_err(), "no interface selected");
    view.field = Field::PhysicalInterface;
    view.key(key(KeyCode::Right));
    assert_eq!(view.nat_interface.as_deref(), Some("Wi-Fi"));
    let request = view.request().unwrap();
    assert_eq!(request.egress, Egress::Interface("Wi-Fi".into()));
    match &request.probe {
        Probe::Nat { request } => {
            assert_eq!(request.agent_id, "agent-1");
            assert!(request.advanced);
            assert_eq!(request.servers.len(), 2);
        }
        other => panic!("unexpected {other:?}"),
    }
    view.field = Field::Family;
    view.key(key(KeyCode::Right));
    assert!(
        view.nat_interface.is_none(),
        "changing family clears the choice"
    );
}

#[tokio::test]
async fn full_check_builds_a_fixed_batch_for_the_analysis_rules() {
    let mut view = view();
    view.set_kind(Kind::FullCheck);
    let batch = view.full_check_request().unwrap();
    assert!(!batch.comparison);
    let ids: Vec<_> = batch.projects.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "a-dns-tcp",
            "a-dns-doh",
            "a-tls",
            "a-http",
            "a-ip4",
            "a-stun"
        ]
    );
    assert!(batch.projects.iter().all(|p| p.path_id == "path-a"));

    view.compare_rounds = true;
    view.dual_stack = true;
    view.field = Field::Comparison;
    view.key(key(KeyCode::Right));
    assert_eq!(view.comparison, Some(1));
    view.include_nat = true;
    view.field = Field::PhysicalInterface;
    view.key(key(KeyCode::Right));
    let batch = view.full_check_request().unwrap();
    assert!(batch.comparison);
    let paths: std::collections::BTreeSet<_> =
        batch.projects.iter().map(|p| p.path_id.as_str()).collect();
    assert_eq!(paths, ["path-a", "path-b", "path-nat"].into());
    assert!(batch.projects.iter().any(|p| p.id == "b-ip6"));
    assert_eq!(
        batch
            .projects
            .iter()
            .filter(|p| matches!(p.request.probe, Probe::Nat { .. }))
            .count(),
        1
    );
    assert!(full_check::build(
        &Egress::System,
        Some(&Egress::System),
        None,
        &Default::default(),
        None
    )
    .is_err());
}

#[derive(serde::Deserialize)]
struct ChainCase {
    name: String,
    runtime: Option<serde_json::Value>,
    network: Option<serde_json::Value>,
    egress: Egress,
    expected: String,
}

/// Shared with the web implementation (`networkChain.test.ts`).
#[test]
fn live_chain_matches_shared_cases() {
    let cases: Vec<ChainCase> =
        serde_json::from_str(include_str!("fixtures/network_chain_cases.json")).unwrap();
    for case in cases {
        let chain = chain::live_chain(case.runtime.as_ref(), case.network.as_ref(), &case.egress);
        assert_eq!(chain.text(), case.expected, "{}", case.name);
    }
}

#[derive(serde::Deserialize)]
struct NatCase {
    name: String,
    report: serde_json::Value,
    expected: String,
}

/// Shared with the web implementation (`networkNat.test.ts`).
#[test]
fn nat_type_matches_shared_cases() {
    let cases: Vec<NatCase> =
        serde_json::from_str(include_str!("fixtures/network_nat_cases.json")).unwrap();
    for case in cases {
        let kind = nat::nat_type(&case.report);
        let name = match &kind {
            nat::NatType::Classified(kind) => serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned(),
            nat::NatType::NoTranslation => "no_translation".into(),
            nat::NatType::Undetermined(_) => "undetermined".into(),
        };
        assert_eq!(name, case.expected, "{}", case.name);
    }
}

#[test]
fn nat_rows_mark_only_the_detected_category() {
    let cases: Vec<NatCase> =
        serde_json::from_str(include_str!("fixtures/network_nat_cases.json")).unwrap();
    let rows = nat_rows(&cases[2].report);
    assert!(rows[0].text.contains("端口受限锥型"));
    let marked: Vec<_> = rows.iter().filter(|r| r.text.starts_with('▶')).collect();
    assert_eq!(marked.len(), 1);
    assert!(marked[0].text.contains("仅已联系的 IP+端口") && marked[0].text.contains("一般"));
    let rows = nat_rows(&cases[5].report);
    assert!(rows.iter().all(|r| !r.text.starts_with('▶')));
    assert!(rows[0].text.contains("未能确定"));
}

#[test]
fn results_are_one_column_and_skip_unknown_events() {
    let mut view = view();
    view.summary = Some(Summary {
        sent: 4,
        received: 3,
        min_ms: Some(1.0),
        avg_ms: Some(2.0),
        max_ms: Some(3.0),
    });
    view.details = vec![
        json!({"event": "removed_kind_from_an_old_report", "x": 1}),
        json!({"event": "http", "seq": 1, "detail": {"kind": "response", "hop": 0,
            "service": "https://example.com", "method": "GET", "status": 200, "version": "HTTP/2",
            "timings": {"dns_ms": 1.0, "transport_connect_ms": 2.0, "tls_ms": 3.0,
                "response_headers_ms": 4.0, "total_ms": 10.0}}}),
        json!({"event": "tls", "seq": 1, "detail": {"kind": "handshake", "server_name": "example.com",
            "protocol": "TLSv1.3", "verified": true}}),
        json!({"event": "trace", "seq": 1, "sample": {"hop": 2, "address": "10.0.0.1", "rtt_ms": 5.0}}),
        json!({"event": "trace", "seq": 2, "sample": {"hop": 1, "address": null, "rtt_ms": null}}),
        json!({"event": "trace", "seq": 3, "sample": {"hop": 2, "address": "10.0.0.1", "rtt_ms": 4.0}}),
        json!({"event": "trace_summary", "target": "example.com", "reached": true}),
        json!({"event": "project", "project": {"project_id": "a-ip4", "round": 1},
            "detail": {"event": "public_ip", "address": "203.0.113.7", "service": "api.ipify.org"}}),
        json!({"event": "analysis", "findings": [{"severity": "risk", "confidence": "high",
            "title": "明文 DNS", "explanation": "UDP 53", "recommendation": "使用 DoH"}],
            "topology": [], "notes": []}),
    ];
    let text = view.output().join("\n");
    assert!(text.contains("发送 4 · 接收 3 · 丢失 25%"));
    assert!(text.contains("HTTP HTTP/2 GET https://example.com → 200"));
    assert!(
        text.contains("TLS 1.0 · 连接 2.0 · TLS 3.0")
            || text.contains("DNS 1.0 · 连接 2.0 · TLS 3.0")
    );
    assert!(text.contains("TLSv1.3 · 证书验证通过"));
    let hop1 = text.find("跳  1").unwrap();
    let hop2 = text.find("跳  2").unwrap();
    assert!(hop1 < hop2 && text.contains("4.00 ms") && !text.contains("5.00 ms"));
    assert!(text.contains("[a-ip4] 公网出口 IP：203.0.113.7"));
    assert!(text.contains("[风险 · 置信度高] 明文 DNS：UDP 53"));
    assert!(!text.contains("removed_kind"));
}

#[test]
fn path_strips_linearise_backend_topology() {
    let analysis = json!({"event": "analysis", "topology": [{
        "path_id": "path-a", "truncated": false,
        "nodes": [
            {"id": "agent", "label": "目标 Agent", "role": "agent"},
            {"id": "interface:en0", "label": "en0", "role": "interface"},
            {"id": "group:PROXY:hk", "label": "PROXY → hk", "role": "proxy_group"},
            {"id": "target", "label": "example.com", "role": "target"},
            {"id": "dns", "label": "1.1.1.1", "role": "dns"}
        ],
        "edges": [
            {"from": "interface:en0", "to": "group:PROXY:hk", "section": "connection", "level": "configuration", "description": "", "evidence": []},
            {"from": "agent", "to": "interface:en0", "section": "connection", "level": "configuration", "description": "", "evidence": []},
            {"from": "group:PROXY:hk", "to": "target", "section": "connection", "level": "measured", "description": "", "evidence": []},
            {"from": "agent", "to": "dns", "section": "dns", "level": "measured", "description": "", "evidence": []}
        ],
        "attachments": [{"kind": "nat", "label": "端口受限锥型", "evidence": []}]
    }]});
    let strips = chain::path_strips(&analysis);
    assert_eq!(strips.len(), 1);
    let rows: Vec<_> = strips[0].rows.iter().map(|r| r.text()).collect();
    assert_eq!(
        rows,
        [
            "[连接] 目标 Agent ┄› en0 ┄› PROXY → hk ─› example.com",
            "[DNS] 目标 Agent ─› 1.1.1.1"
        ]
    );
    assert_eq!(strips[0].rows[0].steps[3].edge, Some(2));
    assert_eq!(strips[0].attachments, ["nat：端口受限锥型"]);
}

#[tokio::test]
async fn late_outbounds_keep_the_selected_egress() {
    let mut view = view();
    view.egress = 1;
    view.set_outbounds(&["DIRECT".into(), "JP".into()]);
    assert_eq!(
        view.egresses[view.egress],
        Egress::Interface("Wi-Fi".into())
    );
    assert_eq!(view.egresses.last(), Some(&Egress::Outbound("JP".into())));
    view.egress = view.egresses.len() - 1;
    view.set_outbounds(&[]);
    assert_eq!(view.egresses[view.egress], Egress::Outbound("JP".into()));
}

#[tokio::test]
async fn keys_navigate_fields_and_esc_leaves_when_idle() {
    let mut view = view();
    view.field = Field::Kind;
    view.key(key(KeyCode::Char('d')));
    assert_eq!(view.kind, Kind::Dns);
    view.field = Field::Count;
    for c in "120".chars() {
        view.key(key(KeyCode::Char(c)));
    }
    assert!(view.count <= MAX_COUNT);
    view.field = Field::Target;
    view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(view.request().is_err());
    view.key(key(KeyCode::Enter));
    assert!(view.error.is_some() && !view.running());
    assert!(
        !view.key(key(KeyCode::Esc)),
        "Esc while idle leaves the page"
    );
}

#[test]
fn host_part_strips_ports_but_not_ipv6() {
    assert_eq!(host_part("example.com:443"), "example.com");
    assert_eq!(host_part("[2001:db8::1]:443"), "2001:db8::1");
    assert_eq!(host_part("2001:db8::1"), "2001:db8::1");
    assert_eq!(host_part("1.1.1.1"), "1.1.1.1");
}

#[test]
fn trace_hops_stay_visible_when_the_trace_never_summarised() {
    let mut view = view();
    view.details = vec![
        json!({"event": "trace", "seq": 1, "sample": {"hop": "1", "address": "192.168.1.1", "rtt_ms": "1.5"}}),
        json!({"event": "trace", "seq": 2, "sample": {"hop": "2", "address": null, "rtt_ms": null}}),
    ];
    let text = view.output().join("\n");
    assert!(
        text.contains("跳  1  192.168.1.1") && text.contains("1.50 ms"),
        "{text}"
    );
    assert!(text.contains("跳  2  *"), "{text}");
}

#[test]
fn space_cycles_the_tool_row_like_other_choices() {
    let mut view = view();
    view.field = Field::Kind;
    view.key(key(KeyCode::Char(' ')));
    assert_eq!(view.kind, Kind::Tcp);
}
