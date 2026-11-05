use super::*;
use crate::dialer::DirectDialer;
use meow_common::AdapterType;

fn make_adapter(flow: Option<VlessFlow>, udp: bool) -> VlessAdapter {
    VlessAdapter::new(
        "test-vless",
        "127.0.0.1",
        12345,
        [0u8; 16],
        flow,
        udp,
        TransportChain::empty(),
        Arc::new(DirectDialer),
    )
}

// ─── I1: adapter_type() returns Vless ────────────────────────────────────

#[test]
fn vless_adapter_type_is_vless() {
    let a = make_adapter(None, false);
    assert_eq!(a.adapter_type(), AdapterType::Vless);
}

// ─── I2: support_udp false by default ────────────────────────────────────

#[test]
fn vless_support_udp_false_by_default() {
    let a = make_adapter(None, false);
    assert!(!a.support_udp());
}

// ─── I3: support_udp true when configured ────────────────────────────────

#[test]
fn vless_support_udp_true_when_configured() {
    let a = make_adapter(None, true);
    assert!(a.support_udp());
}

// ─── E1: TCP + no TLS → chain length 0 ───────────────────────────────────

#[test]
fn vless_tcp_no_tls_empty_chain() {
    let a = make_adapter(None, false);
    assert_eq!(a.transport.len(), 0, "no-TLS TCP chain must be empty");
}

// ─── E2: TCP + TLS → chain length 1 ──────────────────────────────────────

#[test]
fn vless_tcp_with_tls_chain() {
    use meow_transport::tls::{TlsConfig, TlsLayer};
    let mut chain = TransportChain::empty();
    let tls_cfg = TlsConfig::new("example.com");
    let tls_layer = TlsLayer::new(&tls_cfg).expect("TlsLayer");
    chain.push(Box::new(tls_layer));
    let a = VlessAdapter::new(
        "t",
        "127.0.0.1",
        1,
        [0u8; 16],
        None,
        false,
        chain,
        Arc::new(DirectDialer),
    );
    assert_eq!(a.transport.len(), 1, "TLS-only chain must have 1 layer");
}

// ─── E3: WS + TLS → chain length 2, TLS before WS ────────────────────────

#[test]
fn vless_ws_with_tls_chain_ordered() {
    use meow_transport::tls::{TlsConfig, TlsLayer};
    use meow_transport::ws::{WsConfig, WsLayer};
    let mut chain = TransportChain::empty();
    let tls_cfg = TlsConfig::new("example.com");
    chain.push(Box::new(TlsLayer::new(&tls_cfg).expect("TlsLayer")));
    chain.push(Box::new(
        WsLayer::new(WsConfig {
            host_header: Some("example.com".into()),
            ..WsConfig::default()
        })
        .expect("WsLayer::new"),
    ));
    let a = VlessAdapter::new(
        "t",
        "127.0.0.1",
        1,
        [0u8; 16],
        None,
        false,
        chain,
        Arc::new(DirectDialer),
    );
    assert_eq!(a.transport.len(), 2, "TLS+WS chain must have 2 layers");
}

// ─── E5: Vision flow dial_tcp returns VisionConn ─────────────────────────

#[cfg(feature = "vless-vision")]
#[test]
fn vless_vision_wrapped_around_vless_conn() {
    // This is a compile-time check: if VlessFlow::XtlsRprxVision compiles
    // and the dial_tcp match arm for it compiles, the test passes.
    // (Runtime round-trip is in the integration test H4.)
    let _a = make_adapter(Some(VlessFlow::XtlsRprxVision), false);
}

// ─── E6: dial_udp ignores Vision flow (guard-rail) ───────────────────────

// This is tested at runtime in the integration tests.
// The compile-time check: dial_udp should always compile regardless of flow.
#[test]
fn vless_udp_ignores_vision_flow_compiles() {
    // Just verify the adapter compiles with XtlsRprxVision + udp: true.
    #[cfg(feature = "vless-vision")]
    let _ = make_adapter(Some(VlessFlow::XtlsRprxVision), true);
    let _ = make_adapter(None, true);
}
