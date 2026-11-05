use super::*;
use std::io::Write;

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn decodes_identity_gzip_and_deflate_and_rejects_bad_encoding() {
    let text = "proxies: []\n";
    assert_eq!(decode(text.as_bytes(), "identity").unwrap(), text);
    assert_eq!(
        decode(gzip(text.as_bytes()).as_slice(), "GZip").unwrap(),
        text
    );
    let mut deflate = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    deflate.write_all(text.as_bytes()).unwrap();
    assert_eq!(
        decode(deflate.finish().unwrap().as_slice(), "deflate").unwrap(),
        text
    );
    assert!(decode(&b"broken"[..], "gzip").is_err());
    assert!(decode(text.as_bytes(), "br").is_err());
}

#[test]
fn limits_wire_and_expanded_bytes_including_multiple_gzip_members() {
    let large = vec![b'x'; super::super::LIMIT as usize + 1];
    assert!(decode(large.as_slice(), "identity").is_err());
    assert!(decode(gzip(&large).as_slice(), "gzip").is_err());
    let mut members = gzip(&large[..large.len() / 2]);
    members.extend(gzip(&large[large.len() / 2..]));
    assert!(decode(members.as_slice(), "gzip").is_err());
}
