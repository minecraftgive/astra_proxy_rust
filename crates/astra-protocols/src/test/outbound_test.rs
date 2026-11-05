use super::*;
#[test]
fn sock5_domain_is_encoded_with_length() {
    let mut out = Vec::new();
    append_socks_address(
        &mut out,
        &Target {
            host: "abc.test".into(),
            port: 443,
        },
    )
    .unwrap();
    assert_eq!(out, b"\x03\x08abc.test\x01\xbb");
}
