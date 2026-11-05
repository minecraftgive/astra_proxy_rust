use super::*;
#[test]
fn future_document_commands_and_schema_fields_are_rejected() {
    for text in [
        r#"{"command":"future_edit","path":[]}"#,
        r#"{"command":"append_entry","path":[],"future_option":true}"#,
    ] {
        assert!(serde_json::from_str::<EditCommand>(text).is_err());
    }
    let schema = r#"{"core":"proxy","path":[],"collection":false,"rows":[],"future_option":true}"#;
    assert!(serde_json::from_str::<Schema>(schema).is_err());
    assert!(serde_json::from_str::<EditCommand>(r#"{"command":"append_entry","path":[]}"#).is_ok());
}
