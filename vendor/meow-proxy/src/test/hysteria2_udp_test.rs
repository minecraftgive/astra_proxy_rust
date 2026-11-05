use super::*;

fn fragment(packet_id: u16, frag_id: u8, size: usize) -> UdpMessage {
    UdpMessage {
        session_id: 1,
        packet_id,
        frag_id,
        frag_count: 2,
        addr: "localhost:53".into(),
        data: vec![frag_id; size],
    }
}

#[test]
fn reassembly_bounds_duplicates_conflicts_and_expiry() {
    let mut d = UdpDefragger::new();
    let last = fragment(1, 1, 8192);
    assert!(d.feed(last.clone()).unwrap().is_none());
    assert!(d.feed(last).unwrap().is_none());
    assert_eq!(d.packets[&1].bytes, 8192);
    let full = d.feed(fragment(1, 0, 8192)).unwrap().unwrap();
    assert_eq!(full.data.len(), 16384);
    assert_eq!(&full.data[8192..], &[1; 8192]);
    assert!(d.packets.is_empty());

    d.feed(fragment(2, 0, MAX_UDP_SIZE)).unwrap();
    assert!(d.feed(fragment(2, 1, 1)).unwrap().is_none());
    assert!(d.packets.is_empty());
    for id in 0..20 {
        d.feed(fragment(id, 0, 1)).unwrap();
    }
    assert_eq!(d.packets.len(), MAX_PENDING_PACKETS);
    for item in d.packets.values_mut() {
        item.created = Instant::now() - FRAGMENT_TTL - Duration::from_secs(1);
    }
    d.feed(fragment(100, 0, 1)).unwrap();
    assert_eq!(d.packets.len(), 1);
    let mut conflicting = fragment(100, 1, 1);
    conflicting.addr = "other.example:53".into();
    assert!(d.feed(conflicting).unwrap().is_none());
    assert!(d.packets.is_empty());
    let mut empty = fragment(101, 0, 0);
    empty.frag_count = 1;
    assert!(d.feed(empty.clone()).unwrap().unwrap().data.is_empty());
    empty.frag_count = 0;
    assert!(d.feed(empty).unwrap().is_none());
}

#[test]
fn defragger_reassembles_in_order() {
    let mut defragger = UdpDefragger::new();
    let first = UdpMessage {
        session_id: 1,
        packet_id: 2,
        frag_id: 0,
        frag_count: 2,
        addr: "127.0.0.1:53".into(),
        data: b"he".to_vec(),
    };
    let second = UdpMessage {
        frag_id: 1,
        data: b"llo".to_vec(),
        ..first.clone()
    };
    assert!(defragger.feed(first).unwrap().is_none());
    let complete = defragger.feed(second).unwrap().unwrap();
    assert_eq!(complete.data, b"hello");
}
