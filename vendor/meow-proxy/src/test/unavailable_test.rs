use super::*;

fn fixture() -> UnavailableAdapter {
    UnavailableAdapter::new(
        "vpn-tokyo",
        "trusttunnel is not compiled into this build",
        AdapterType::TrustTunnel,
    )
}

#[test]
fn reports_the_node_name_and_is_dead() {
    let adapter = fixture();
    assert_eq!(adapter.name(), "vpn-tokyo");
    assert!(!adapter.health().alive());
    // Advertised so the rule engine keeps the match and fails the UDP
    // dial, instead of skipping past the node to DIRECT.
    assert!(adapter.support_udp());
    // The declared type, not `Reject`: a node that failed to parse must
    // not read as an intentional reject in the API listing, in match
    // statistics, or in the group dial-failure exemptions.
    assert_eq!(adapter.adapter_type(), AdapterType::TrustTunnel);
}

#[tokio::test]
async fn every_dial_fails_naming_the_node_and_reason() {
    let adapter = fixture();
    let metadata = Metadata::default();
    for error in [
        adapter.dial_tcp(&metadata).await.err(),
        adapter.dial_udp(&metadata).await.err(),
    ] {
        let error = error.expect("an unavailable node must never produce a connection");
        let rendered = error.to_string();
        assert!(rendered.contains("vpn-tokyo"), "{rendered}");
        assert!(
            rendered.contains("not compiled into this build"),
            "{rendered}"
        );
    }
}
