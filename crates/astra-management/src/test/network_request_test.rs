use super::*;

#[test]
fn new_request_keeps_legacy_json_fields_and_defaults_the_resolver() {
    let legacy = NetToolRequest {
        egress: astra_model::nettool::Egress::System,
        probe: astra_model::nettool::Probe::Tcp {
            host: "example.test".into(),
            port: 443,
        },
        count: 1,
        interval_ms: "0".into(),
        timeout_ms: "1000".into(),
    };
    let value = serde_json::to_value(&legacy).unwrap();
    assert!(value.get("resolver").is_none());
    let detailed: NetworkDiagnosticsRequest = serde_json::from_value(value.clone()).unwrap();
    assert!(detailed.resolver.is_none());
    let mut roundtrip = serde_json::to_value(detailed).unwrap();
    roundtrip.as_object_mut().unwrap().remove("resolver");
    roundtrip.as_object_mut().unwrap().remove("family");
    assert_eq!(roundtrip, value);
}

#[test]
fn public_request_cannot_supply_a_pin_or_forge_interface_resolvers() {
    let request: NetworkDiagnosticsRequest = serde_json::from_value(serde_json::json!({
        "egress":{"kind":"outbound","name":"group"},
        "probe":{"kind":"tcp","host":"example.test","port":443},
        "count":1,"interval_ms":"0","timeout_ms":"1000",
        "resolver":"udp://192.0.2.53",
        "pinned":{"version":1,"leaf_tag":"DIRECT"},
        "interface_resolvers":[{"source":"client-controlled"}],
        "config":"credential=do-not-trust", "path_context":{"core_tun":false},
        "policy":{"matched_running":true}
    }))
    .unwrap();
    let core = request.into_core().unwrap();
    assert_eq!(core.resolver.as_deref(), Some("udp://192.0.2.53"));
    assert!(core.pinned.is_none());
    assert!(core.interface_resolvers.is_empty());
    assert!(core.config.is_empty());
    assert!(core.path_context.is_none());
}

#[test]
fn typed_web_probe_fixtures_roundtrip_through_single_and_batch_commands() {
    let values: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/network_requests.json")).unwrap();
    assert_eq!(values.len(), 10);
    for value in values {
        let request: NetworkDiagnosticsRequest = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), value);
        let core = request.clone().into_core().unwrap();
        assert_eq!(serde_json::to_value(&core.probe).unwrap(), value["probe"]);
        assert_eq!(core.timeout_ms, 1000);
        assert_eq!(core.interval_ms, 0);
        assert!(
            core.pinned.is_none()
                && core.path_context.is_none()
                && core.interface_resolvers.is_empty()
        );
        let single = serde_json::json!({"type":"network_diagnostics","data":{"instance":"proxy","request":value}});
        let command: Command = serde_json::from_value(single.clone()).unwrap();
        assert_eq!(serde_json::to_value(command).unwrap(), single);
        let batch = serde_json::json!({"type":"network_diagnostics_batch","data":{"instance":"proxy","request":{"comparison":false,"projects":[{"id":"probe","path_id":"selected","request":value}]}}});
        let command: Command = serde_json::from_value(batch.clone()).unwrap();
        assert_eq!(serde_json::to_value(command).unwrap(), batch);
    }
}
#[test]
fn typed_requests_reject_wrong_discriminators_missing_fields_and_numeric_overflow() {
    let values: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/network_requests.json")).unwrap();
    for probe in [
        serde_json::json!({"kind":"unknown"}),
        serde_json::json!({"kind":"tcp","host":"test"}),
        serde_json::json!({"kind":"tcp","host":"test","port":65536}),
        serde_json::json!({"kind":"tcp","host":"test","port":-1}),
        serde_json::json!({"kind":"trace","host":"test","max_hops":256,"probes_per_hop":3}),
    ] {
        let mut value = values[1].clone();
        value["probe"] = probe;
        assert!(serde_json::from_value::<NetworkDiagnosticsRequest>(value).is_err());
    }
    for (field, wrong) in [
        ("count", serde_json::json!(-1)),
        ("count", serde_json::json!(4294967296_u64)),
        ("timeout_ms", serde_json::json!(1000)),
        ("family", serde_json::json!("auto")),
    ] {
        let mut value = values[1].clone();
        value[field] = wrong;
        assert!(serde_json::from_value::<NetworkDiagnosticsRequest>(value).is_err());
    }
    for text in ["-1", "1.5", "18446744073709551616", "not-a-number"] {
        let mut value = values[1].clone();
        value["timeout_ms"] = text.into();
        assert!(serde_json::from_value::<NetworkDiagnosticsRequest>(value)
            .unwrap()
            .into_core()
            .is_err());
    }
}
