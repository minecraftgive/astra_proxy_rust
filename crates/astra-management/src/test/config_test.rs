use super::*;
#[test]
fn deployment_round_trip_and_safe_defaults() {
    let original = ManagerConfig::default();
    let encoded = toml::to_string(&original).unwrap();
    assert!(encoded.contains("role = \"agent\""));
    assert!(encoded.contains("[storage]"));
    assert!(encoded.contains("[upstream]"));
    let mut decoded: ManagerConfig = toml::from_str(&encoded).unwrap();
    assert!(decoded.validate().is_ok());
    assert!(!decoded.web.enabled);
    decoded.web.enabled = true;
    decoded.web.public_origin = Some("http://0.0.0.0:8787".into());
    decoded.web.dev_http = true;
    decoded.web.listen = "0.0.0.0:8787".parse().unwrap();
    assert!(decoded.validate().is_err());
}
#[test]
fn unrecognized_deployment_is_not_silently_reset() {
    assert!(toml::from_str::<ManagerConfig>("role='agent'\nmode='agent'\n").is_err());
}
