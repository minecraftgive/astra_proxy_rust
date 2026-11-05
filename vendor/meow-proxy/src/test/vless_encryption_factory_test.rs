use super::*;

#[test]
fn none_and_empty_return_none() {
    assert!(parse_client_encryption("").unwrap().is_none());
    assert!(parse_client_encryption("none").unwrap().is_none());
}

#[test]
fn rejects_non_mlkem() {
    assert!(parse_client_encryption("aes-128-gcm").is_err());
    assert!(parse_client_encryption("mlkem768x25519plus.bogus.1rtt.KEY").is_err());
}

#[test]
fn parses_sample_config_key() {
    // 32-byte X25519 public key, base64 raw-url (from the issue's config style).
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 32]);
    let enc = format!("mlkem768x25519plus.native.0rtt.{key}");
    let inst = parse_client_encryption(&enc).unwrap().unwrap();
    assert_eq!(inst.xor_mode(), 0);
    assert_eq!(inst.seconds(), 1);
}

#[test]
fn rejects_wrong_key_length() {
    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7u8; 40]);
    let enc = format!("mlkem768x25519plus.native.1rtt.{key}");
    assert!(parse_client_encryption(&enc).is_err());
}

#[test]
fn accepts_issue_301_key_with_noncanonical_trailing_bits() {
    // The exact `encryption` value from issue #301: Go's RawURLEncoding
    // decodes this 32-byte key despite non-canonical trailing bits, so we
    // must too.
    let enc = "mlkem768x25519plus.native.0rtt.DA7B2WRj7X2zGFwMelbIbcaoUrpLjzoPpmydYW8NvQW";
    let inst = parse_client_encryption(enc).expect("issue #301 key must parse");
    assert!(inst.is_some());
}
