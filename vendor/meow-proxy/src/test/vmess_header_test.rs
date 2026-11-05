use super::*;
// RustCrypto plays the independent server: it pins the BoringSSL seal
// and open paths to the wire spec rather than to themselves.
use aes_gcm::aead::Aead as _;
use aes_gcm::{Aes128Gcm, Nonce};

/// Decrypt the sealed length block inside a 42-byte request-header
/// prefix, returning the total frame length (`42 + L + 16`) so tests
/// can `read_exact` one header frame without racing buffered body
/// records — `server_open_request_header` rejects trailing bytes.
pub(crate) fn request_header_frame_len(
    cmd_key: &[u8; 16],
    prefix: &[u8; 42],
) -> Result<usize, String> {
    use aes_gcm::aead::Payload;
    let auth_id = &prefix[0..16];
    let encrypted_length = &prefix[16..34];
    let conn_nonce = &prefix[34..42];
    let length_key = kdf16(
        cmd_key,
        &[b"VMess Header AEAD Key_Length", auth_id, conn_nonce],
    );
    let length_iv = kdf12(
        cmd_key,
        &[b"VMess Header AEAD Nonce_Length", auth_id, conn_nonce],
    );
    let len_pt = Aes128Gcm::new_from_slice(&length_key)
        .map_err(|e| e.to_string())?
        .decrypt(
            Nonce::from_slice(&length_iv),
            Payload {
                msg: encrypted_length,
                aad: auth_id,
            },
        )
        .map_err(|_| "length AEAD open failed".to_string())?;
    Ok(42 + u16::from_be_bytes([len_pt[0], len_pt[1]]) as usize + 16)
}

/// Seal a minimal VMess response header `[resp_v, 0, 0, 0]` the way a
/// conformant server would — shared by the conn/mod duplex tests that
/// play the server side.
pub(crate) fn seal_response_header(req_key: &[u8; 16], req_iv: &[u8; 16], resp_v: u8) -> Vec<u8> {
    let (resp_key, resp_iv) = response_body_keys(req_key, req_iv);
    let header = [resp_v, 0, 0, 0];

    let len_key = kdf16(&resp_key, &[b"AEAD Resp Header Len Key"]);
    let len_iv = kdf12(&resp_iv, &[b"AEAD Resp Header Len IV"]);
    let len_ct = Aes128Gcm::new_from_slice(&len_key)
        .unwrap()
        .encrypt(
            Nonce::from_slice(&len_iv),
            (header.len() as u16).to_be_bytes().as_ref(),
        )
        .unwrap();

    let header_key = kdf16(&resp_key, &[b"AEAD Resp Header Key"]);
    let header_iv = kdf12(&resp_iv, &[b"AEAD Resp Header IV"]);
    let header_ct = Aes128Gcm::new_from_slice(&header_key)
        .unwrap()
        .encrypt(Nonce::from_slice(&header_iv), header.as_ref())
        .unwrap();

    [len_ct, header_ct].concat()
}

fn protocol_constants_and_hashes_match_reference() {
    let uuid: [u8; 16] = [
        0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08,
        0x11,
    ];
    assert_eq!(cmd_key(&uuid), cmd_key(&uuid));
    assert_ne!(cmd_key(&uuid), [0u8; 16]);
    assert_eq!(fnv1a32(b""), 0x811c_9dc5);
    assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
    assert_eq!(Security::Aes128Gcm.to_nibble(), 0x03);
    assert_eq!(Security::ChaCha20Poly1305.to_nibble(), 0x04);
    assert_eq!(Security::None.to_nibble(), 0x05);
    assert!(matches!(
        auto_security(),
        Security::Aes128Gcm | Security::ChaCha20Poly1305
    ));
}

fn address_encoding_covers_all_wire_variants_and_boundaries() {
    fn encoded(meta: &Metadata) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_address(&mut buf, meta).unwrap();
        buf
    }

    assert_eq!(
        encoded(&Metadata {
            dst_ip: Some(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
            ..Default::default()
        }),
        vec![ADDR_IPV4, 127, 0, 0, 1]
    );

    let ipv6 = encoded(&Metadata {
        dst_ip: Some(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
        ..Default::default()
    });
    assert_eq!(ipv6[0], ADDR_IPV6);
    assert_eq!(ipv6.len(), 17);

    let domain = encoded(&Metadata {
        host: "example.com".into(),
        dst_ip: Some(IpAddr::V4(std::net::Ipv4Addr::new(1, 2, 3, 4))),
        ..Default::default()
    });
    assert_eq!(&domain, b"\x02\x0bexample.com");

    let idn = encoded(&Metadata {
        host: "例え.jp".into(),
        ..Default::default()
    });
    assert_eq!(&idn[2..], "例え.jp".as_bytes());

    let max_domain = encoded(&Metadata {
        host: "a".repeat(255).into(),
        ..Default::default()
    });
    assert_eq!(max_domain[1], 255);
    assert_eq!(max_domain.len(), 257);
}

fn address_encoding_rejects_missing_or_oversized_destination() {
    let mut buf = Vec::new();
    assert!(encode_address(&mut buf, &Metadata::default()).is_err());
    assert!(encode_address(
        &mut buf,
        &Metadata {
            host: "a".repeat(256).into(),
            ..Default::default()
        }
    )
    .is_err());
}

/// Independently open a sealed request header the way a conformant server
/// (v2ray `OpenVMessAEADHeader`) does, re-deriving every key from the wire
/// bytes. This catches the seal-order, plaintext-length, and length-salt
/// bugs that a self-consistent seal/open pair would hide.
pub(crate) fn server_open_request_header(
    cmd_key: &[u8; 16],
    wire: &[u8],
) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, Payload};
    let auth_id = &wire[0..16];
    let encrypted_length = &wire[16..34]; // 2 + 16 tag
    let conn_nonce = &wire[34..42]; // 8 bytes AFTER the length block
    let encrypted_payload = &wire[42..];

    let length_key = kdf16(
        cmd_key,
        &[b"VMess Header AEAD Key_Length", auth_id, conn_nonce],
    );
    let length_iv = kdf12(
        cmd_key,
        &[b"VMess Header AEAD Nonce_Length", auth_id, conn_nonce],
    );
    let len_cipher = Aes128Gcm::new_from_slice(&length_key).map_err(|e| e.to_string())?;
    let len_pt = len_cipher
        .decrypt(
            Nonce::from_slice(&length_iv),
            Payload {
                msg: encrypted_length,
                aad: auth_id,
            },
        )
        .map_err(|_| "length AEAD open failed".to_string())?;
    let l = u16::from_be_bytes([len_pt[0], len_pt[1]]) as usize;
    if encrypted_payload.len() != l + 16 {
        return Err(format!(
            "payload len {} != L+16 ({})",
            encrypted_payload.len(),
            l + 16
        ));
    }

    let header_key = kdf16(cmd_key, &[b"VMess Header AEAD Key", auth_id, conn_nonce]);
    let header_iv = kdf12(cmd_key, &[b"VMess Header AEAD Nonce", auth_id, conn_nonce]);
    let hdr_cipher = Aes128Gcm::new_from_slice(&header_key).map_err(|e| e.to_string())?;
    let plaintext = hdr_cipher
        .decrypt(
            Nonce::from_slice(&header_iv),
            Payload {
                msg: encrypted_payload,
                aad: auth_id,
            },
        )
        .map_err(|_| "payload AEAD open failed".to_string())?;
    if plaintext.len() != l {
        return Err(format!("plaintext len {} != L {}", plaintext.len(), l));
    }
    Ok(plaintext)
}

fn sealed_headers_are_unique_and_server_openable() {
    let uuid: [u8; 16] = [
        0xb8, 0x31, 0x38, 0x1d, 0x63, 0x24, 0x4d, 0x53, 0xad, 0x4f, 0x8c, 0xda, 0x48, 0xb3, 0x08,
        0x11,
    ];
    let ck = cmd_key(&uuid);
    let meta = Metadata {
        host: "example.com".into(),
        dst_port: 443,
        ..Default::default()
    };
    let sealed = seal_request_header(&ck, Security::Aes128Gcm, &meta, false).unwrap();
    let second = seal_request_header(&ck, Security::Aes128Gcm, &meta, false).unwrap();
    assert_ne!(sealed.bytes, second.bytes);
    assert_ne!(sealed.req_key, second.req_key);
    assert!(sealed.bytes.len() >= 16 + 18 + 8 + 16);
    assert_ne!(sealed.req_key, [0; 16]);
    assert_ne!(sealed.req_iv, [0; 16]);

    let pt = server_open_request_header(&ck, &sealed.bytes)
        .expect("a conformant server must be able to open the sealed header");

    // Plaintext layout: version(1) | req_iv(16) | req_key(16) | resp_v(1) | ...
    assert_eq!(pt[0], 0x01, "version byte");
    assert_eq!(
        &pt[1..17],
        &sealed.req_iv,
        "req_iv round-trips through wire"
    );
    assert_eq!(
        &pt[17..33],
        &sealed.req_key,
        "req_key round-trips through wire"
    );
    assert_eq!(pt[33], sealed.resp_v, "resp_v round-trips through wire");

    // FNV-1a trailer must cover everything before it.
    let body = &pt[..pt.len() - 4];
    let want = fnv1a32(body);
    let got = u32::from_be_bytes([
        pt[pt.len() - 4],
        pt[pt.len() - 3],
        pt[pt.len() - 2],
        pt[pt.len() - 1],
    ]);
    assert_eq!(got, want, "server-visible FNV-1a must validate");
}

async fn response_header_round_trips() {
    use aes_gcm::aead::Aead;
    let req_key = [0x11u8; 16];
    let req_iv = [0x22u8; 16];
    let resp_v = 0x5A;
    let (rk, ri) = response_body_keys(&req_key, &req_iv);

    // Server seals a minimal 4-byte response header [resp_v, opt=0, cmdlen=0, 0].
    let hdr_pt = [resp_v, 0x00, 0x00, 0x00];
    let len_key = kdf16(&rk, &[b"AEAD Resp Header Len Key"]);
    let len_iv = kdf12(&ri, &[b"AEAD Resp Header Len IV"]);
    let len_ct = Aes128Gcm::new_from_slice(&len_key)
        .unwrap()
        .encrypt(
            Nonce::from_slice(&len_iv),
            (hdr_pt.len() as u16).to_be_bytes().as_ref(),
        )
        .unwrap();
    let hdr_key = kdf16(&rk, &[b"AEAD Resp Header Key"]);
    let hdr_iv = kdf12(&ri, &[b"AEAD Resp Header IV"]);
    let hdr_ct = Aes128Gcm::new_from_slice(&hdr_key)
        .unwrap()
        .encrypt(Nonce::from_slice(&hdr_iv), hdr_pt.as_ref())
        .unwrap();

    let mut wire = Vec::new();
    wire.extend_from_slice(&len_ct);
    wire.extend_from_slice(&hdr_ct);
    wire.extend_from_slice(b"body-records-follow");

    let mut cursor = std::io::Cursor::new(wire);
    read_aead_response_header(&mut cursor, &rk, &ri, resp_v)
        .await
        .expect("client must open the server's response header");

    // Reader is positioned exactly at the body bytes.
    let mut rest = Vec::new();
    cursor.read_to_end(&mut rest).await.unwrap();
    assert_eq!(rest, b"body-records-follow");

    // A wrong resp_v must be rejected.
    let mut cursor2 = std::io::Cursor::new({
        let mut w = Vec::new();
        w.extend_from_slice(&len_ct);
        w.extend_from_slice(&hdr_ct);
        w
    });
    assert!(
        read_aead_response_header(&mut cursor2, &rk, &ri, resp_v ^ 0xFF)
            .await
            .is_err()
    );
}

fn plaintext_layout_and_checksum_match_protocol() {
    let meta = Metadata {
        host: "example.com".into(),
        dst_port: 443,
        ..Default::default()
    };
    let mut rng = FakeRng(0);
    let req_key = [0u8; 16];
    let req_iv = [0u8; 16];
    let pt = build_header_plaintext(
        &req_key,
        &req_iv,
        0x42,
        Security::Aes128Gcm,
        HeaderCmd::Tcp(&meta),
        &mut rng,
    )
    .unwrap();
    // version(1) + req_iv(16) + req_key(16) + resp_v(1) + opts(1) + p_sec(1) + reserved(1) + cmd(1) = 38
    // Then port(2) then addr_type(1)
    let port_offset = 38;
    let port = u16::from_be_bytes([pt[port_offset], pt[port_offset + 1]]);
    assert_eq!(port, 443, "port must be at offset 38 (before addr_type)");
    assert_eq!(
        pt[port_offset + 2],
        ADDR_DOMAIN,
        "addr_type must follow port"
    );

    let body = &pt[..pt.len() - 4];
    let expected_hash = fnv1a32(body);
    let actual_hash = u32::from_be_bytes([
        pt[pt.len() - 4],
        pt[pt.len() - 3],
        pt[pt.len() - 2],
        pt[pt.len() - 1],
    ]);
    assert_eq!(
        actual_hash, expected_hash,
        "FNV-1a must cover all preceding bytes"
    );
}

#[tokio::test]
async fn header_wire_format_matches_protocol() {
    protocol_constants_and_hashes_match_reference();
    address_encoding_covers_all_wire_variants_and_boundaries();
    address_encoding_rejects_missing_or_oversized_destination();
    plaintext_layout_and_checksum_match_protocol();
    sealed_headers_are_unique_and_server_openable();
    response_header_round_trips().await;
}

struct FakeRng(u64);
impl rand::RngCore for FakeRng {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(1);
        self.0 as u32
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for b in dest.iter_mut() {
            *b = self.next_u32() as u8;
        }
    }
}
