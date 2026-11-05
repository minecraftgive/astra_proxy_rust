use super::*;

/// Regression for the dependent-crate full-run flake (seen on macOS in
/// `meow-tunnel`'s process-enrichment tests): `pbi_nfiles` is a
/// snapshot, `PROC_PIDLISTFDS` truncates to the buffer — under parallel
/// fd churn the just-bound socket's entry fell off the tail and the
/// lookup missed it. `pid_fds` must grow past a stale count.
#[cfg(target_os = "macos")]
#[test]
fn pid_fds_grows_past_stale_snapshot_count() {
    use std::os::fd::AsRawFd;
    // Hold enough fds that the floor-clamped initial buffer (16)
    // provably cannot fit the table — `len() > 16` then demonstrates
    // the doubling retry actually ran, not merely the clamp floor.
    let _held: Vec<std::net::TcpListener> = (0..24)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let marker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let marker_fd = marker.as_raw_fd();
    let fds = platform::pid_fds(std::process::id() as i32, 1).expect("own pid must list fds");
    assert!(
        fds.len() > 16,
        "stale count must grow past the initial cap: {} entries",
        fds.len()
    );
    assert!(
        fds.iter().any(|f| f.proc_fd == marker_fd),
        "the just-bound socket fd must appear"
    );
}

#[test]
fn finds_self_via_tcp_listener() {
    // Bind a TCP listener on 127.0.0.1:<ephemeral> and then ask
    // `find_process` who owns that endpoint — it must be this test binary.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let info = find_process(Network::Tcp, addr)
        .expect("process lookup should locate the current test process");
    // Win32 process lookup does not populate uid.
    #[cfg(not(target_os = "windows"))]
    assert!(info.uid.is_some(), "uid should be populated");
    // Exact-match guard-rail: the returned name must equal the full test
    // binary filename. On Linux this catches `/proc/<pid>/comm` truncation
    // (TASK_COMM_LEN=16 → 15-char cap) which mangles `<crate>-<16hex>`
    // cargo-test harness names — the bug fixed by 65f19e5.
    let expected = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .expect("current_exe should be readable in tests");
    assert_eq!(info.name, expected, "process name must not be truncated");
}

#[test]
fn finds_self_via_udp_socket() {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    let info = find_process(Network::Udp, addr)
        .expect("process lookup should locate the current test process for UDP");
    assert!(!info.name.is_empty());
}

#[test]
fn tcp_ipv6_lookup() {
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let info = find_process(Network::Tcp, addr).expect("TCP IPv6 process lookup should succeed");
    assert!(!info.name.is_empty());
}

#[test]
fn udp_ipv6_lookup() {
    let sock = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let addr = sock.local_addr().unwrap();
    let info = find_process(Network::Udp, addr).expect("UDP IPv6 process lookup should succeed");
    assert!(!info.name.is_empty());
}

#[test]
fn udp_wildcard_binds_match() {
    // UDP sockets typically bind 0.0.0.0:port. A lookup targeting
    // 127.0.0.1:<same_port> should still match via the wildcard rule.
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    let actual = sock.local_addr().unwrap();
    // Query with 127.0.0.1 instead of 0.0.0.0 — the row address is
    // unspecified, so the wildcard match path is exercised.
    let target = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        actual.port(),
    );
    let info = find_process(Network::Udp, target)
        .expect("UDP wildcard bind lookup should match via unspecified-address path");
    assert!(!info.name.is_empty());
}

#[test]
fn unknown_endpoint_returns_none() {
    // Port 1 is reserved and should not be bound by any test-run process.
    let fake = "127.0.0.1:1".parse().unwrap();
    assert!(find_process(Network::Tcp, fake).is_none());
}

/// The blocking-pool wrapper must return exactly what the synchronous
/// scan returns for the same endpoint (issue #515).
#[tokio::test]
async fn find_process_async_parity() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let sync = find_process(Network::Tcp, addr);
    let async_result = find_process_async(Network::Tcp, addr).await;
    assert_eq!(sync.is_some(), async_result.is_some());
    if let (Some(s), Some(a)) = (sync, async_result) {
        assert_eq!(s.name, a.name);
        assert_eq!(s.path, a.path);
        assert_eq!(s.uid, a.uid);
    }
}

/// The Linux cache layer itself (issue #515). `sock_table`'s slots are
/// shared process-wide, so fixture coverage goes through the pure
/// parser rather than the cache — populating a slot with fixture data
/// would poison parallel tests for `TABLE_TTL`.
#[cfg(target_os = "linux")]
mod linux_cache {
    use super::super::platform;
    use std::io::Write;

    #[test]
    fn parse_proc_net_reads_fixture() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Header line + one row: 127.0.0.1:8080 (0100007F:1F90), uid 0,
        // inode 4242.
        writeln!(
            f,
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when \
                 retrnsmt   uid  timeout inode"
        )
        .unwrap();
        writeln!(
            f,
            "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 \
                 00000000     0        0 4242 1 0000000000000000 100 0 0 10 0"
        )
        .unwrap();
        let table = platform::parse_proc_net(f.path().to_str().unwrap(), false)
            .expect("fixture must parse");
        let rows = table.get(&8080).expect("port 8080 row");
        assert_eq!(rows[0].0, "127.0.0.1".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(rows[0].1, 4242, "inode");
        assert_eq!(rows[0].2, 0, "uid");

        // An unreadable path yields None → cached as a lookup miss.
        assert!(platform::parse_proc_net("/nonexistent", false).is_none());
    }

    #[test]
    fn inode_cache_returns_none_for_unknown() {
        // u64::MAX never matches a real inode; the miss is cached.
        assert!(platform::find_pid_by_inode_cached(u64::MAX).is_none());
    }
}
