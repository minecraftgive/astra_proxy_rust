use super::*;
#[test]
fn parse_proc_columns_without_allocating_fields() {
    let socket = parse_socket(
        " 0: 0100007F:1234 00000000:0000 07 00000000:00000000 00:00000000 00000000 1000 0 42 2",
    )
    .unwrap();
    assert_eq!(socket.local, "127.0.0.1:4660".parse().unwrap());
    assert_eq!(socket.inode, 42);
    assert!(parse_socket("short row").is_none());
}
#[test]
fn shared_inodes_and_multiple_owners_remain_ambiguous() {
    let mut owners = Owners::new();
    add_owner(&mut owners, 1, 10);
    add_owner(&mut owners, 1, 10);
    assert_eq!(unique_owner(&owners, &HashSet::from([1])), Some(10));
    add_owner(&mut owners, 2, 20);
    assert_eq!(unique_owner(&owners, &HashSet::from([1, 2])), None);
    add_owner(&mut owners, 1, 20);
    add_owner(&mut owners, 1, 10);
    assert_eq!(unique_owner(&owners, &HashSet::from([1])), None);
}
#[test]
fn forced_refresh_reuses_snapshots_taken_after_the_lookup_started() {
    let start = Instant::now();
    let before = start - Duration::from_millis(1);
    assert!(reusable(start, Some(start)));
    assert!(!reusable(before, Some(start)));
    assert!(reusable(before, None));
    assert!(!reusable(start - SNAPSHOT_TTL, None));
}
#[test]
fn overflow_fallback_scans_only_requested_inodes() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let local = listener.local_addr().unwrap();
    let table = socket_table(Network::Tcp, Some(Instant::now())).unwrap();
    let inodes: HashSet<u64> = table[&local.port()]
        .iter()
        .filter(|socket| socket.local == local)
        .map(|socket| socket.inode)
        .collect();
    assert_eq!(scan_owner(&inodes), Some(std::process::id()));
    assert_eq!(scan_owner(&HashSet::from([u64::MAX])), None);
}
#[test]
fn wildcard_udp_matching_keeps_remote_semantics() {
    let socket = Socket {
        local: "0.0.0.0:1234".parse().unwrap(),
        remote: "0.0.0.0:0".parse().unwrap(),
        inode: 42,
    };
    let table = SocketTable::from([(1234, vec![socket])]);
    let source = "127.0.0.1:1234".parse().unwrap();
    let destination = "203.0.113.1:443".parse().unwrap();
    assert_eq!(
        matching_inodes(&table, Network::Udp, source, destination),
        HashSet::from([42])
    );
    assert!(matching_inodes(&table, Network::Tcp, source, destination).is_empty());
}
