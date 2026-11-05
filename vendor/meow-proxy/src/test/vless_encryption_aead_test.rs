use super::*;

#[test]
fn nonce_increments_big_endian_and_wraps() {
    let mut a = Aead::new(b"ctx", b"key", true);
    assert_eq!(a.nonce, [0u8; 12]);
    a.increment_nonce();
    assert_eq!(a.nonce[11], 1);
    // Force a carry.
    a.nonce = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF];
    a.increment_nonce();
    assert_eq!(a.nonce, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
    // Wrap from max to zero.
    a.nonce = MAX_NONCE;
    assert!(a.is_exhausted());
    a.increment_nonce();
    assert_eq!(a.nonce, [0u8; 12]);
}

/// The hand-rolled context hash must reproduce `blake3::derive_key`
/// bit-for-bit — checkable on any valid UTF-8 context, across every
/// chunk-boundary length and tree depth.
#[test]
fn byte_context_derive_matches_str_derive_key() {
    let mut rng_state = 0x9E3779B97F4A7C15u64;
    let mut next = || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        rng_state
    };
    for len in [
        0usize, 1, 63, 64, 65, 127, 1023, 1024, 1025, 1087, 2047, 2048, 2049, 3000, 4096,
        7168,  // 7 chunks — popcount 3: exercises the final-fold re-push
        15360, // 15 chunks — popcount 4: three re-push iterations
        16645, // VLESS rekey ctx: 5-byte header + a max-size record
        8192,
    ] {
        // Valid UTF-8 by construction: ASCII bytes only.
        let ctx: String = (0..len)
            .map(|_| ((next() % 95) as u8 + 32) as char)
            .collect();
        let material: Vec<u8> = (0..64).map(|_| next() as u8).collect();
        assert_eq!(
            derive_key(ctx.as_bytes(), &material),
            blake3::derive_key(&ctx, &material),
            "context length {len}"
        );
    }
}

/// The whole point: contexts that are not UTF-8 (IVs, keys, ciphertext)
/// must not error or panic — and two different binary contexts derive
/// different keys.
#[test]
fn byte_context_derive_accepts_arbitrary_bytes() {
    let a = derive_key(&[0xFF; 32], b"key");
    let b = derive_key(&[0xFE; 32], b"key");
    assert_ne!(a, b);
    // ~16 KiB of invalid-UTF-8 bytes — exercises the multi-chunk tree.
    let big = vec![0x80u8; 16 * 1024 + 7];
    let _ = derive_key(&big, b"key");
}

#[test]
fn aead_round_trip_both_ciphers() {
    for use_aes in [true, false] {
        let mut enc = Aead::new(b"iv", b"key", use_aes);
        let mut dec = Aead::new(b"iv", b"key", use_aes);
        let header = b"\x17\x03\x03\x00\x1b";
        let mut out = b"prefix".to_vec();
        enc.seal_ad_append(b"hello world", header, &mut out);
        let mut ct = out.split_off(b"prefix".len());
        dec.open_ad_in_place(&mut ct, header).unwrap();
        assert_eq!(ct, b"hello world");
    }
}

/// RustCrypto, keyed with the same BLAKE3-derived key, must open what
/// `seal_ad`/`seal_max` emit and seal what `open_ad`/`open_max` accept —
/// pins key selection, nonce counting and AD handling to the wire
/// format, which the loopback tests (one implementation on both ends)
/// cannot.
#[test]
fn records_interoperate_with_independent_aead() {
    use aes_gcm::aead::{Aead as _, KeyInit, Payload};
    use aes_gcm::Aes256Gcm;
    use chacha20poly1305::ChaCha20Poly1305;

    let (ctx, key, hdr) = (b"iv", b"key", b"\x17\x03\x03\x00\x1b");
    let k = derive_key(ctx, key);
    let counter = |n: u8| {
        let mut nonce = [0u8; 12];
        nonce[11] = n;
        nonce
    };
    for use_aes in [true, false] {
        let seal = |nonce: [u8; 12], msg: &[u8], aad: &[u8]| {
            let payload = Payload { msg, aad };
            if use_aes {
                Aes256Gcm::new((&k).into()).encrypt((&nonce).into(), payload)
            } else {
                ChaCha20Poly1305::new((&k).into()).encrypt((&nonce).into(), payload)
            }
            .unwrap()
        };
        let open = |nonce: [u8; 12], msg: &[u8], aad: &[u8]| {
            let payload = Payload { msg, aad };
            if use_aes {
                Aes256Gcm::new((&k).into()).decrypt((&nonce).into(), payload)
            } else {
                ChaCha20Poly1305::new((&k).into()).decrypt((&nonce).into(), payload)
            }
            .unwrap()
        };

        let mut ours = Aead::new(ctx, key, use_aes);
        let first = ours.seal_ad(b"first", hdr);
        let second = ours.seal(b"second");
        assert_eq!(open(counter(1), &first, hdr), b"first");
        assert_eq!(open(counter(2), &second, b""), b"second");
        assert_eq!(open(MAX_NONCE, &ours.seal_max(b"max"), b""), b"max");

        let mut peer = Aead::new(ctx, key, use_aes);
        let reply = seal(counter(1), b"reply", hdr);
        assert_eq!(peer.open_ad(&reply, hdr).unwrap(), b"reply");
        let next = seal(counter(2), b"next", b"");
        assert_eq!(peer.open(&next).unwrap(), b"next");
        let max = seal(MAX_NONCE, b"max", b"");
        assert_eq!(peer.open_max(&max).unwrap(), b"max");
    }
}

#[test]
fn header_round_trip_and_bounds() {
    let h = encode_header(20);
    assert_eq!(decode_header(&h).unwrap(), 20);
    // Too short / too long / wrong prefix are rejected.
    assert!(decode_header(&encode_header(16)).is_err());
    assert!(decode_header(&[0, 3, 3, 0, 20]).is_err());
}

#[test]
fn parse_padding_rejects_short_first() {
    assert!(parse_padding("10-20-30").is_err());
    assert!(parse_padding("100-111-1111.75-0-111").is_ok());
    assert!(parse_padding("").is_ok());
}

#[test]
fn parse_padding_bounded_against_overflow() {
    // Two i64::MAX lens endpoints would wrap `max_len` back under the
    // cap without `checked_add` (issue #648).
    let huge = format!("100-{m}-{m}.1-0-0.1-{m}-{m}", m = i64::MAX);
    assert!(parse_padding(&huge).is_err());
    // A single over-cap lens value is caught by the existing bound.
    assert!(parse_padding("100-35-999999").is_err());
    // Gaps get their own bound — a giant ms value is a dial-hang.
    assert!(parse_padding("100-111-1111.75-0-99999999").is_err());
    assert!(parse_padding("100-111-1111.75-0-111").is_ok());
}
