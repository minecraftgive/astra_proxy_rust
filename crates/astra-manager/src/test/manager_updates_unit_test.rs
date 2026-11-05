use super::*;
#[test]
fn online_activation_requires_an_ordinary_user_manager() {
    assert!(require_unprivileged_update(false).is_ok());
    let error = require_unprivileged_update(true).unwrap_err().to_string();
    assert!(error.contains("elevated manager"));
    assert!(error.contains("existing connections are unchanged"));
}
