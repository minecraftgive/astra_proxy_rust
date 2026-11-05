use super::*;

#[test]
fn kdf_matches_single_and_multi_segment_reference_vectors() {
    assert_eq!(
        kdf(b"key", &[b"label"]),
        hex_literal_32("c9cebf77e859ffcbe78619d4e503b0df707f1d7ac98a189c418763940880e3eb")
    );
    assert_eq!(
        kdf(
            b"Demo Key for KDF Value Test",
            &[
                b"Demo Path for KDF Value Test",
                b"Demo Path for KDF Value Test2",
                b"Demo Path for KDF Value Test3",
            ],
        ),
        hex_literal_32("53e9d7e1bd7bd25022b71ead07d8a596efc8a845c7888652fd684b4903dc8892")
    );
}

fn hex_literal_32(s: &str) -> [u8; 32] {
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for (i, chunk) in bytes.chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).unwrap() as u8;
        let lo = (chunk[1] as char).to_digit(16).unwrap() as u8;
        out[i] = (hi << 4) | lo;
    }
    out
}
