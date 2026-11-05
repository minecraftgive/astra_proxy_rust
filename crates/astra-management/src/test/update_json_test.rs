#[test]
fn v2_preserves_large_revisions_and_normal_version_numbers() {
    let mut status = crate::updates::UpdateStatus {
        observed_at_ms: u64::MAX,
        ..Default::default()
    };
    status
        .source_rule_revisions
        .insert("rules".into(), u64::MAX);
    let response = crate::v2::ResponseBody::UpdateStatus(Box::new(status));
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["data"]["observed_at_ms"], u64::MAX.to_string());
    assert_eq!(
        json["data"]["source_rule_revisions"]["rules"],
        u64::MAX.to_string()
    );
    assert_eq!(json["data"]["schema_version"], 1);
    let crate::v2::ResponseBody::UpdateStatus(decoded) = serde_json::from_value(json).unwrap()
    else {
        panic!()
    };
    assert_eq!(decoded.observed_at_ms, u64::MAX);
    assert_eq!(decoded.source_rule_revisions["rules"], u64::MAX);
}
