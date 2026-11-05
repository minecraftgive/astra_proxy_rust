use super::*;
fn options() -> Reality {
    Reality {
        private_key: URL_SAFE_NO_PAD.encode([8; 32]),
        server_names: vec!["example.test".into()],
        short_ids: vec!["0102030405060708".into()],
        target: "example.test:443".into(),
        max_time_diff_secs: 60,
    }
}
fn hello(name: &str, short: [u8; 8], time: u64) -> Vec<u8> {
    let client = x25519_dalek::StaticSecret::from(rand::random::<[u8; 32]>());
    let public = x25519_dalek::PublicKey::from(&client);
    let server = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([8; 32]));
    let mut body = vec![1, 0, 0, 0, 3, 3];
    body.extend(rand::random::<[u8; 32]>());
    body.push(32);
    body.resize(71, 0);
    body.extend([0, 2, 0x13, 1, 1, 0]);
    let mut name_ext = vec![];
    name_ext.extend(((name.len() + 3) as u16).to_be_bytes());
    name_ext.push(0);
    name_ext.extend((name.len() as u16).to_be_bytes());
    name_ext.extend(name.as_bytes());
    let mut extensions = vec![0, 0];
    extensions.extend((name_ext.len() as u16).to_be_bytes());
    extensions.extend(name_ext);
    extensions.extend([0, 51, 0, 38, 0, 36, 0, 29, 0, 32]);
    extensions.extend(public.as_bytes());
    body.extend((extensions.len() as u16).to_be_bytes());
    body.extend(extensions);
    let len = body.len() - 4;
    body[1] = (len >> 16) as u8;
    body[2] = (len >> 8) as u8;
    body[3] = len as u8;
    let shared = client.diffie_hellman(&server);
    let mut key = [0; 32];
    hkdf::Hkdf::<Sha256>::new(Some(&body[6..26]), shared.as_bytes())
        .expand(b"REALITY", &mut key)
        .unwrap();
    let mut payload = vec![1, 0, 0, 0];
    payload.extend((time as u32).to_be_bytes());
    payload.extend(short);
    let tag = Aes256Gcm::new_from_slice(&key)
        .unwrap()
        .encrypt_in_place_detached(Nonce::from_slice(&body[26..38]), &body, &mut payload)
        .unwrap();
    body[39..55].copy_from_slice(&payload);
    body[55..71].copy_from_slice(&tag);
    body
}
#[test]
fn reality_authenticates_and_rejects_replay_time_sni_and_short_id() {
    let state = RealityState::new(options(), None).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let valid = hello("example.test", [1, 2, 3, 4, 5, 6, 7, 8], now);
    assert!(state.authenticate(&valid).is_ok());
    assert!(state
        .authenticate(&valid)
        .unwrap_err()
        .to_string()
        .contains("replay"));
    let reloaded = RealityState::new(options(), Some(&state)).unwrap();
    assert!(reloaded.authenticate(&valid).is_err());
    assert!(state
        .authenticate(&hello("other.test", [1, 2, 3, 4, 5, 6, 7, 8], now))
        .is_err());
    assert!(state
        .authenticate(&hello("example.test", [9; 8], now))
        .is_err());
    assert!(state
        .authenticate(&hello("example.test", [1, 2, 3, 4, 5, 6, 7, 8], now - 120))
        .is_err());
    for end in 0..valid.len() {
        assert!(parse_hello(&valid[..end]).is_err(), "truncation {end}");
    }
    let mut damaged = hello("example.test", [1, 2, 3, 4, 5, 6, 7, 8], now);
    damaged[39] ^= 1;
    assert!(state.authenticate(&damaged).is_err());
}
#[tokio::test]
async fn record_gate_handles_tiny_reads_without_consuming_raw_successor() {
    let (mut input, inner) = tokio::io::duplex(128);
    let first = [23, 3, 3, 0, 3, 1, 2, 3];
    let raw = [23, 3, 3, 0, 2, 9, 8];
    input.write_all(&first).await.unwrap();
    input.write_all(&raw).await.unwrap();
    let mut gate = RecordIo {
        inner: Box::new(inner),
        prefix: std::io::Cursor::new(vec![]),
        header: [0; 5],
        header_pos: 0,
        header_delivered: 0,
        left: 0,
    };
    let mut received = vec![];
    for _ in 0..first.len() {
        received.push(gate.read_u8().await.unwrap());
    }
    assert_eq!(received, first);
    let mut next = [0; 7];
    gate.inner.read_exact(&mut next).await.unwrap();
    assert_eq!(next, raw);
}
