use super::*;

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap()
}

/// Plaintext and AAD of McGrew–Viega GCM spec test cases 4 and 16.
const GCM_SPEC_IV: &str = "cafebabefacedbaddecaf888";
const GCM_SPEC_AD: &str = "feedfacedeadbeeffeedfacedeadbeefabaddad2";
const GCM_SPEC_PLAIN: &str = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d\
                                  8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39";

/// Seal `plain` both ways — detached tag and appended tag — and check
/// each against `sealed` (ciphertext || tag), then open both back.
/// Pins the implementation to a published vector, independently of the
/// protocol round-trip tests that share one implementation.
fn assert_vector(aead: &Aead, nonce: &str, ad: &str, plain: &str, sealed: &str) {
    let nonce: [u8; NONCE_LEN] = unhex(nonce).try_into().unwrap();
    let (ad, plain, sealed) = (unhex(ad), unhex(plain), unhex(sealed));
    let (want_ct, want_tag) = sealed.split_at(plain.len());

    let mut data = plain.clone();
    let mut tag = [0u8; TAG_LEN];
    aead.seal_detached(&nonce, &ad, &mut data, &mut tag)
        .unwrap();
    assert_eq!((data.as_slice(), tag.as_slice()), (want_ct, want_tag));
    aead.open_detached(&nonce, &ad, &mut data, &tag).unwrap();
    assert_eq!(data, plain);

    let mut buf = plain.clone();
    aead.seal_append(&nonce, &ad, &mut buf).unwrap();
    assert_eq!(buf, sealed);
    aead.open_trailing(&nonce, &ad, &mut buf).unwrap();
    assert_eq!(buf, plain);
}

#[cfg(any(feature = "snell", feature = "vmess"))]
#[test]
fn aes_128_gcm_matches_gcm_spec() {
    // Test case 2: zero key and nonce, one zero block, no AAD.
    assert_vector(
        &Aead::aes_128_gcm(&[0; 16]),
        "000000000000000000000000",
        "",
        "00000000000000000000000000000000",
        "0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf",
    );
    // Test case 4: 60-byte plaintext with AAD.
    assert_vector(
        &Aead::aes_128_gcm(&unhex("feffe9928665731c6d6a8f9467308308")),
        GCM_SPEC_IV,
        GCM_SPEC_AD,
        GCM_SPEC_PLAIN,
        "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e23\
             29aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091\
             5bc94fbc3221a5db94fae95ae7121a47",
    );
}

#[cfg(feature = "vless-encryption")]
#[test]
fn aes_256_gcm_matches_gcm_spec() {
    // Test case 14: zero key and nonce, one zero block, no AAD.
    assert_vector(
        &Aead::aes_256_gcm(&[0; 32]),
        "000000000000000000000000",
        "",
        "00000000000000000000000000000000",
        "cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919",
    );
    // Test case 16: 60-byte plaintext with AAD.
    assert_vector(
        &Aead::aes_256_gcm(&unhex(
            "feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308",
        )),
        GCM_SPEC_IV,
        GCM_SPEC_AD,
        GCM_SPEC_PLAIN,
        "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa\
             8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662\
             76fc6ece0f4e1768cddf8853bb2d551b",
    );
}

/// RFC 8439 §2.8.2.
#[cfg(any(feature = "vmess", feature = "vless-encryption"))]
#[test]
fn chacha20_poly1305_matches_rfc8439() {
    let key: Vec<u8> = (0x80..0xa0).collect();
    assert_vector(
        &Aead::chacha20_poly1305(&key),
        "070000004041424344454647",
        "50515253c0c1c2c3c4c5c6c7",
        &hex::encode(
            "Ladies and Gentlemen of the class of '99: If I could offer you only one \
                 tip for the future, sunscreen would be it.",
        ),
        "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
             3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
             92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
             3ff4def08e4b7a9de576d26586cec64b6116\
             1ae10b594f09e26a7e902ecbd0600691",
    );
}

#[cfg(any(feature = "snell", feature = "vmess"))]
#[test]
fn open_rejects_tampering_and_short_input() {
    let aead = Aead::aes_128_gcm(&[7; 16]);
    let nonce = [1; NONCE_LEN];
    let mut buf = b"record".to_vec();
    aead.seal_append(&nonce, b"ad", &mut buf).unwrap();

    let mut flipped = buf.clone();
    flipped[0] ^= 1;
    assert!(aead.open_trailing(&nonce, b"ad", &mut flipped).is_err());
    assert!(aead
        .open_trailing(&[2; NONCE_LEN], b"ad", &mut buf.clone())
        .is_err());
    assert!(aead
        .open_trailing(&nonce, b"other ad", &mut buf.clone())
        .is_err());
    assert!(aead
        .open_trailing(&nonce, b"", &mut vec![0; TAG_LEN - 1])
        .is_err());
    aead.open_trailing(&nonce, b"ad", &mut buf).unwrap();
    assert_eq!(buf, b"record");
}
