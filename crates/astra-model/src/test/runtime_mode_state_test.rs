use super::*;
#[test]
fn switches_default_off_and_are_serialized_separately_from_profiles() {
    let state = Overrides::default();
    assert_eq!(state.system_proxy, Some(false));
    assert_eq!(state.tun, Some(false));
    assert_eq!(state.allow_lan, Some(false));
    let decoded: Overrides = serde_json::from_str("{}").unwrap();
    assert_eq!(decoded.system_proxy, Some(false));
    assert_eq!(decoded.tun, Some(false));
    let saved = serde_json::to_string(&Overrides {
        system_proxy: Some(true),
        tun: Some(true),
        allow_lan: Some(true),
        ..Default::default()
    })
    .unwrap();
    let restored: Overrides = serde_json::from_str(&saved).unwrap();
    assert_eq!(restored.system_proxy, Some(true));
    assert_eq!(restored.tun, Some(true));
}
