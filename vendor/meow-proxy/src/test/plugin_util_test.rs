use super::*;

#[test]
fn sip003_opts_tokenizes_and_lowercases_keys() {
    let got: Vec<_> = sip003_opts(" Host=example.com ;TLS; path = /x ;").collect();
    assert_eq!(
        got,
        [
            ("host".to_string(), "example.com".to_string()),
            ("tls".to_string(), "true".to_string()),
            ("path".to_string(), "/x".to_string()),
        ]
    );
    // A value may itself contain `=` (base64, query strings).
    assert_eq!(
        sip003_opts("k=a=b=c").collect::<Vec<_>>(),
        [("k".to_string(), "a=b=c".to_string())]
    );
    assert_eq!(sip003_opts("  ;; ").next(), None);
}

#[test]
fn parse_bool_lenient_vs_strict() {
    assert!(parse_bool("YES", "p", "k"));
    assert!(!parse_bool("bogus", "p", "k"));
    assert!(parse_bool_strict("on", "p", "k").unwrap());
    assert!(parse_bool_strict("bogus", "p", "k").is_err());
    // Strict also rejects the lenient empty-string coercion.
    assert!(parse_bool_strict("", "p", "k").is_err());
}

#[test]
fn read_cert_file_caps_oversized_files() {
    // A provider-controlled `certificate`/`private-key` path reaches this
    // read — a swapped-in huge regular file must not exhaust memory
    // (issue #648).
    let dir = std::env::temp_dir().join(format!("meow-pem-cap-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.pem");
    std::fs::write(&big, vec![b'x'; (MAX_PEM_FILE_BYTES + 1) as usize]).unwrap();
    assert!(read_cert_file(&big).is_err());
    let small = dir.join("small.pem");
    std::fs::write(
        &small,
        b"-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    assert!(read_cert_file(&small).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cert_pin_rejects_utls_names_and_bad_hex() {
    assert!(parse_cert_pin("chrome", "p").is_err());
    assert!(parse_cert_pin("zz".repeat(32).as_str(), "p").is_err());
    assert!(parse_cert_pin(&"ab".repeat(31), "p").is_err()); // 31 bytes
    assert!(
        parse_cert_pin(&"ab".repeat(32), "p").is_ok(),
        "64 hex chars without colons"
    );
    assert!(parse_cert_pin(&"ab:".repeat(32)[..95], "p").is_ok());
}
