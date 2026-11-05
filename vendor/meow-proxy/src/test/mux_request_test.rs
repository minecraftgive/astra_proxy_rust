use super::*;

#[test]
fn v0_header_round_trips() {
    let req = Request::new(VERSION_0, 0, false);
    let bytes = req.encode();
    assert_eq!(bytes.len(), 2);
    let (decoded, consumed) = Request::decode(&bytes).unwrap();
    assert_eq!(decoded, req);
    assert_eq!(consumed, 2);
}

#[test]
fn v1_no_padding_round_trips() {
    let req = Request::new(VERSION_1, 1, false);
    let bytes = req.encode();
    assert_eq!(bytes.len(), 3);
    let (decoded, consumed) = Request::decode(&bytes).unwrap();
    assert_eq!(decoded, req);
    assert_eq!(consumed, 3);
}

#[test]
fn v1_padding_skips_padding_bytes() {
    let req = Request::new(VERSION_1, 2, true);
    let bytes = req.encode();
    assert!(bytes.len() > 3 + 256);
    let (decoded, consumed) = Request::decode(&bytes).unwrap();
    assert_eq!(decoded, req);
    assert_eq!(consumed, bytes.len());
}

#[test]
fn rejects_bad_version() {
    let bytes = [9u8, 0];
    assert!(Request::decode(&bytes).is_err());
}

#[test]
fn rejects_unknown_protocol() {
    let bytes = [0u8, 7];
    assert!(Request::decode(&bytes).is_err());
}
