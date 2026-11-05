use super::*;
#[test]
fn matching_proxy_requires_all_managed_properties() {
    let original = Settings {
        flags: 3,
        server: Some("127.0.0.1:8080".into()),
        bypass: Some("<local>".into()),
        auto_config_url: None,
    };
    let mut actual = original.clone();
    actual.auto_config_url = Some(String::new());
    assert!(matches_expected(&actual, &original));
    actual.server = Some("another.proxy:3128".into());
    assert!(!matches_expected(&actual, &original));
    actual = original.clone();
    actual.flags = 1;
    assert!(!matches_expected(&actual, &original));
    actual = original.clone();
    actual.auto_config_url = Some("https://example.invalid/pac".into());
    assert!(!matches_expected(&actual, &original));
}
