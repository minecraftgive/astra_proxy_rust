use super::*;
#[tokio::test]
async fn quic_fragments_reorder_and_preserve_empty_datagrams() {
    for protocol in [Protocol::Tuic, Protocol::Hysteria2] {
        for size in [0, 1, 8192, 65507] {
            let target = Target {
                host: "::1".into(),
                port: 53,
            };
            let payload = vec![42; size];
            let parts = encode(7, 9, &target, &payload, protocol, 1200).unwrap();
            if protocol == Protocol::Tuic && parts.len() > 1 {
                assert_eq!(parts[1][10], 255);
            }
            let mut state = Fragments::default();
            let mut result = None;
            for part in parts.into_iter().rev() {
                if let Some(packet) = decode(&part, protocol, &mut state).await.unwrap() {
                    result = Some(packet);
                }
            }
            let (id, to, data) = result.unwrap();
            assert_eq!(id, 7);
            assert_eq!(to, target);
            assert_eq!(data, payload);
            assert!(state.entries.is_empty());
        }
    }
}
#[test]
fn fragment_state_rejects_conflicts_and_bounds_retained_payload() {
    let mut state = Fragments::default();
    let target = Target {
        host: "example.test".into(),
        port: 53,
    };
    assert!(state
        .insert(1, 1, 2, 0, Some(target.clone()), b"hello")
        .unwrap()
        .is_none());
    assert!(state.insert(1, 1, 2, 0, None, b"hello").unwrap().is_none());
    assert!(state.insert(1, 1, 2, 0, None, b"changed").is_err());
    assert!(state.insert(1, 1, 3, 1, None, b"x").is_err());
    assert!(state
        .insert(1, 2, 2, 0, Some(target), &vec![0; 65507])
        .unwrap()
        .is_none());
    assert!(state.insert(1, 2, 2, 1, None, b"x").is_err());
}
