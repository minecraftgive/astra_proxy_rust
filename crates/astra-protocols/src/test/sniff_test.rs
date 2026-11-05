use super::*;
fn vector(bytes: &[u8]) -> Vec<u8> {
    let mut out = (bytes.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(bytes);
    out
}
fn handshake() -> Vec<u8> {
    let mut name = vec![0];
    name.extend(vector(b"QQ.COM"));
    let names = vector(&name);
    let mut extension = vec![0, 0];
    extension.extend(vector(&names));
    let mut body = vec![3, 3];
    body.extend([0; 32]);
    body.push(0);
    body.extend([0, 2, 0x13, 1, 1, 0]);
    body.extend(vector(&extension));
    let mut hello = vec![1, 0, (body.len() >> 8) as u8, body.len() as u8];
    hello.extend(body);
    hello
}
#[test]
fn reads_tls_sni_across_records_and_rejects_bad_lengths() {
    let hello = handshake();
    let mut bytes = vec![22, 3, 1];
    bytes.extend(vector(&hello[..15]));
    bytes.extend([22, 3, 3]);
    bytes.extend(vector(&hello[15..]));
    assert_eq!(inspect(&bytes), Result::Host("qq.com".into()));
    for end in 0..bytes.len() {
        assert_eq!(inspect(&bytes[..end]), Result::NeedMore);
    }
    assert_eq!(inspect(&[22, 3, 3, 255, 255]), Result::Unknown);
    for n in 0..512 {
        let bytes = vec![(n % 256) as u8; n];
        let _ = inspect(&bytes);
    }
}
#[test]
fn reads_http_host_without_consuming_or_accepting_ambiguous_headers() {
    assert_eq!(
        inspect(b"GET / HTTP/1.1\r\nhOsT: QQ.com:80\r\n\r\nbody"),
        Result::Host("qq.com".into())
    );
    assert_eq!(
        inspect(b"GET / HTTP/1.1\r\nHost: qq.com\r\nHost: github.com\r\n\r\n"),
        Result::Unknown
    );
    assert_eq!(
        inspect(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
        Result::Unknown
    );
    assert_eq!(inspect(b"SSH-2.0"), Result::Unknown);
    assert_eq!(inspect(b"GET / HTTP/1.1\r\nHost:"), Result::NeedMore);
}
