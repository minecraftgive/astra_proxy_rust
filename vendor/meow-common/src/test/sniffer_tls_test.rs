use super::*;

fn build_client_hello(hostname: &str) -> Vec<u8> {
    let name_bytes = hostname.as_bytes();
    let sni_entry_len = 3 + name_bytes.len();
    let sni_list_len = sni_entry_len;
    let sni_ext_data_len = 2 + sni_list_len;

    let mut sni_ext = Vec::new();
    sni_ext.extend_from_slice(&[0x00, 0x00]);
    sni_ext.extend_from_slice(&(sni_ext_data_len as u16).to_be_bytes());
    sni_ext.extend_from_slice(&(sni_list_len as u16).to_be_bytes());
    sni_ext.push(0x00);
    sni_ext.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
    sni_ext.extend_from_slice(name_bytes);

    let extensions_len = sni_ext.len();

    let mut hello = Vec::new();
    hello.extend_from_slice(&[0x03, 0x03]);
    hello.extend_from_slice(&[0u8; 32]);
    hello.push(0x00);
    hello.extend_from_slice(&[0x00, 0x02, 0x00, 0x2f]);
    hello.extend_from_slice(&[0x01, 0x00]);
    hello.extend_from_slice(&(extensions_len as u16).to_be_bytes());
    hello.extend_from_slice(&sni_ext);

    let handshake_len = hello.len();

    let mut handshake = vec![
        0x01,
        ((handshake_len >> 16) & 0xff) as u8,
        ((handshake_len >> 8) & 0xff) as u8,
        (handshake_len & 0xff) as u8,
    ];
    handshake.extend_from_slice(&hello);

    let record_len = handshake.len();

    let mut record = Vec::new();
    record.push(0x16);
    record.extend_from_slice(&[0x03, 0x01]);
    record.extend_from_slice(&(record_len as u16).to_be_bytes());
    record.extend_from_slice(&handshake);

    record
}

#[test]
fn sniff_tls_sni_extraction_cases() {
    // `very.long.subdomain.example.co.uk` is longer than the 23-byte SmolStr
    // inline limit, so it also covers the heap-allocated return path; the two
    // shorter names stay inlined. Keep all three literals.
    for host in [
        "example.com",
        "www.google.com",
        "very.long.subdomain.example.co.uk",
    ] {
        let data = build_client_hello(host);
        assert_eq!(
            sniff_tls(&data).as_deref(),
            Some(host),
            "SNI extraction failed for hostname {host:?}"
        );
    }
}

#[test]
fn test_no_sni_on_non_tls() {
    let data = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
    assert_eq!(sniff_tls(data), None);
}

#[test]
fn test_no_sni_on_empty() {
    assert_eq!(sniff_tls(&[]), None);
}

#[test]
fn test_no_sni_on_truncated() {
    let data = build_client_hello("example.com");
    assert_eq!(sniff_tls(&data[..data.len() / 2]), None);
}

#[test]
fn test_no_sni_wrong_content_type() {
    let mut data = build_client_hello("example.com");
    data[0] = 0x17;
    assert_eq!(sniff_tls(&data), None);
}

#[test]
fn sniff_tls_partial_record_header_only() {
    // Only the 5-byte TLS record header, no handshake body — must not panic.
    let header = [0x16u8, 0x03, 0x01, 0x00, 0x05];
    assert_eq!(sniff_tls(&header), None);
}

/// Build a ClientHello whose only extension is a non-SNI extension
/// (extended_master_secret, type 0x0017, empty data). Sniffing should
/// return `None` rather than panicking on the missing SNI.
fn build_client_hello_without_sni() -> Vec<u8> {
    let mut other_ext = Vec::new();
    other_ext.extend_from_slice(&[0x00, 0x17]);
    other_ext.extend_from_slice(&[0x00, 0x00]);

    let extensions_len = other_ext.len();

    let mut hello = Vec::new();
    hello.extend_from_slice(&[0x03, 0x03]);
    hello.extend_from_slice(&[0u8; 32]);
    hello.push(0x00);
    hello.extend_from_slice(&[0x00, 0x02, 0x00, 0x2f]);
    hello.extend_from_slice(&[0x01, 0x00]);
    hello.extend_from_slice(&(extensions_len as u16).to_be_bytes());
    hello.extend_from_slice(&other_ext);

    let handshake_len = hello.len();
    let mut handshake = vec![
        0x01,
        ((handshake_len >> 16) & 0xff) as u8,
        ((handshake_len >> 8) & 0xff) as u8,
        (handshake_len & 0xff) as u8,
    ];
    handshake.extend_from_slice(&hello);

    let record_len = handshake.len();
    let mut record = Vec::new();
    record.push(0x16);
    record.extend_from_slice(&[0x03, 0x01]);
    record.extend_from_slice(&(record_len as u16).to_be_bytes());
    record.extend_from_slice(&handshake);
    record
}

#[test]
fn sniff_tls_no_sni_extension_returns_none() {
    let data = build_client_hello_without_sni();
    assert_eq!(sniff_tls(&data), None);
}

#[test]
fn sniff_tls_skips_non_host_name_entries() {
    // SNI extension with a non-zero name_type entry first, then a host_name
    // entry. Per RFC 6066 §3 only name_type=0 (host_name) is currently
    // defined, so the parser must skip the unknown entry rather than
    // misinterpreting it as the hostname.
    let host = b"target.example.com";
    // Entry 1: name_type=0x99 (unknown), name="ignored"
    let unknown = b"ignored";
    // Entry 2: name_type=0x00 (host_name), name=host
    let mut list = Vec::new();
    list.push(0x99);
    list.extend_from_slice(&(unknown.len() as u16).to_be_bytes());
    list.extend_from_slice(unknown);
    list.push(0x00);
    list.extend_from_slice(&(host.len() as u16).to_be_bytes());
    list.extend_from_slice(host);

    let mut sni_ext = Vec::new();
    sni_ext.extend_from_slice(&[0x00, 0x00]);
    let ext_data_len = (2 + list.len()) as u16;
    sni_ext.extend_from_slice(&ext_data_len.to_be_bytes());
    sni_ext.extend_from_slice(&(list.len() as u16).to_be_bytes());
    sni_ext.extend_from_slice(&list);

    let extensions_len = sni_ext.len();
    let mut hello = Vec::new();
    hello.extend_from_slice(&[0x03, 0x03]);
    hello.extend_from_slice(&[0u8; 32]);
    hello.push(0x00);
    hello.extend_from_slice(&[0x00, 0x02, 0x00, 0x2f]);
    hello.extend_from_slice(&[0x01, 0x00]);
    hello.extend_from_slice(&(extensions_len as u16).to_be_bytes());
    hello.extend_from_slice(&sni_ext);

    let handshake_len = hello.len();
    let mut handshake = vec![
        0x01,
        ((handshake_len >> 16) & 0xff) as u8,
        ((handshake_len >> 8) & 0xff) as u8,
        (handshake_len & 0xff) as u8,
    ];
    handshake.extend_from_slice(&hello);

    let record_len = handshake.len();
    let mut record = Vec::new();
    record.push(0x16);
    record.extend_from_slice(&[0x03, 0x01]);
    record.extend_from_slice(&(record_len as u16).to_be_bytes());
    record.extend_from_slice(&handshake);

    assert_eq!(sniff_tls(&record).as_deref(), Some("target.example.com"));
}
