#[test]
fn retains_causes_and_location_but_never_parser_source_or_url_credentials() {
    let parse = toml::from_str::<toml::Table>("password = 'node-secret' trailing").unwrap_err();
    let error = anyhow::Error::new(parse).context("validate merged subscription");
    let report = super::error(&error);
    assert!(
        report.contains("validate merged subscription")
            && report.contains("TOML:")
            && report.contains("bytes")
    );
    assert!(!report.contains("node-secret"));
    let error = anyhow::anyhow!("HTTP 403 https://user:pass@example.com/private-token?token=secret#fragment password=credential uuid='identifier' invalid value \"node-secret\"").context("download provider");
    let report = super::error(&error);
    assert!(
        report.contains("HTTP 403 https://example.com") && report.contains("download provider")
    );
    for secret in [
        "user:pass",
        "private-token",
        "secret#",
        "credential",
        "identifier",
        "node-secret",
    ] {
        assert!(!report.contains(secret), "{report}");
    }
    assert!(super::error(&anyhow::anyhow!("x".repeat(10000))).len() <= 4096);
}
