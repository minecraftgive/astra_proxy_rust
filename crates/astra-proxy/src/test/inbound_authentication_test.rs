use super::*;
#[test]
fn credentials_are_required_and_duplicate_or_invalid_headers_are_rejected() {
    let users = vec!["alice:correct:password".into()];
    let good = format!("Basic {}", STANDARD.encode("alice:correct:password"));
    assert!(http_authorized(&[&good], &users));
    assert!(!http_authorized(&[], &users));
    assert!(!http_authorized(&[&good, &good], &users));
    assert!(!http_authorized(&["Basic !!!!"], &users));
    assert!(!http_authorized(&["Basic YWxpY2U6d3Jvbmc="], &users));
}
