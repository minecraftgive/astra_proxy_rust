use super::*;
#[tokio::test]
async fn poisoned_associations_wake_readers_and_cannot_reuse_partial_state() {
    let associations = Associations::default();
    let (send, mut receive) = tokio::sync::mpsc::channel(1);
    associations.lock().unwrap().insert(1, send);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = associations.entries.lock().unwrap();
        panic!("injected association update panic");
    }))
    .is_err());
    assert!(associations.lock().is_err());
    assert!(tokio::time::timeout(Duration::from_secs(1), receive.recv())
        .await
        .unwrap()
        .is_none());
    // Clearing the poisoned mutex does not make this failed session reusable.
    assert!(!associations.entries.is_poisoned());
    assert!(associations.lock().is_err());
}

fn fragment(id: u16, count: u8, index: u8, target: Option<&Target>, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![5, 2, 0, 0];
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(&[count, index]);
    frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    if let Some(target) = target {
        encode_address(&mut frame, target).unwrap();
    } else {
        frame.push(255);
    }
    frame.extend_from_slice(payload);
    frame
}

#[test]
fn fragmented_udp_reorders_and_rejects_conflicts_without_unbounded_buffers() {
    let (_, receive) = tokio::sync::mpsc::channel(1);
    let mut reader = PacketReader {
        receive,
        fragments: std::collections::HashMap::new(),
    };
    let target = Target {
        host: "example.test".into(),
        port: 53,
    };
    let first = fragment(7, 2, 0, Some(&target), b"hello");
    let last = fragment(7, 2, 1, None, b" world");
    assert!(reader.accept(decode_packet(&last).unwrap()).is_none());
    assert!(reader.accept(decode_packet(&last).unwrap()).is_none());
    assert_eq!(
        reader.accept(decode_packet(&first).unwrap()).unwrap(),
        (target.clone(), b"hello world".to_vec())
    );
    assert!(reader.fragments.is_empty());
    reader.accept(decode_packet(&first).unwrap());
    let conflict = fragment(7, 3, 1, None, b"invalid");
    assert!(reader.accept(decode_packet(&conflict).unwrap()).is_none());
    assert!(reader.fragments.is_empty());
    for id in 0..20 {
        reader.accept(decode_packet(&fragment(id, 2, 0, Some(&target), b"x")).unwrap());
    }
    assert_eq!(reader.fragments.len(), 8);
    for assembly in reader.fragments.values_mut() {
        assembly.created = std::time::Instant::now() - Duration::from_secs(6);
    }
    reader.expire();
    assert!(reader.fragments.is_empty());
    let large = vec![0; MAX_UDP];
    reader.accept(decode_packet(&fragment(1, 2, 0, Some(&target), &large)).unwrap());
    assert!(reader
        .accept(decode_packet(&fragment(1, 2, 1, None, b"x")).unwrap())
        .is_none());
    assert!(reader.fragments.is_empty());
    let full = fragment(9, 2, 0, Some(&target), &large);
    reader.accept(decode_packet(&full).unwrap());
    reader.accept(decode_packet(&full).unwrap());
    let completed = reader
        .accept(decode_packet(&fragment(9, 2, 1, None, b"")).unwrap())
        .unwrap();
    assert_eq!(completed.1.len(), MAX_UDP);
}

#[test]
fn udp_framing_validates_lengths_and_addresses() {
    for host in ["example.test", "127.0.0.1", "::1"] {
        let target = Target {
            host: host.into(),
            port: 53,
        };
        let frame = fragment(3, 1, 0, Some(&target), b"");
        assert_eq!(decode_packet(&frame).unwrap().target, Some(target));
        for end in 0..frame.len() {
            assert!(decode_packet(&frame[..end]).is_err());
        }
        let mut malformed = frame.clone();
        malformed[6] = 0;
        assert!(decode_packet(&malformed).is_err());
        malformed = frame.clone();
        malformed.push(0);
        assert!(decode_packet(&malformed).is_err());
    }
}
