use super::*;

#[test]
fn tickets_compare_without_length_leaks() {
    assert!(ticket_eq("abc", "abc"));
    assert!(!ticket_eq("abc", "abd"));
    assert!(!ticket_eq("abc", "ab"));
    let value = ticket().unwrap();
    assert_eq!(value.len(), 32);
    assert_ne!(value, ticket().unwrap());
}

#[test]
fn handoff_preserves_explicit_resident_mode() {
    for hosted in [false, true] {
        let handoff = Handoff {
            service_owner: crate::lifecycle::capture(std::process::id()).unwrap(),
            version: PROTOCOL_VERSION,
            config: "/private/config.toml".into(),
            control: "/private/control.sock".into(),
            log: "/private/core.log".into(),
            log_level: "info".into(),
            runtime_dir: "/private/runtime".into(),
            cwd: "/private".into(),
            digest: 7,
            uid: 1000,
            gid: 1000,
            hosted,
        };
        let bytes = frame(&handoff).unwrap();
        let decoded: Handoff = decode(&bytes[4..]).unwrap();
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.hosted, hosted);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn peer_credentials_come_from_the_kernel() {
    use std::os::fd::AsRawFd;

    let path = std::env::temp_dir().join(format!("astra-peer-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let client = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let (server, _) = listener.accept().unwrap();
    let from_client = peer_credentials(client.as_raw_fd()).unwrap();
    let from_server = peer_credentials(server.as_raw_fd()).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(from_client.uid, unsafe { libc::geteuid() });
    assert_eq!(from_client.pid, std::process::id() as i32);
    assert_eq!(from_server.uid, from_client.uid);
    assert_eq!(from_server.pid, from_client.pid);
}

#[tokio::test]
async fn async_frames_round_trip() {
    let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
    let offer = Offer {
        version: PROTOCOL_VERSION,
        socket: "/tmp/example.sock".into(),
        ticket: "t".into(),
        binary: "/bin/astra-proxy".into(),
        sudo: "/usr/bin/sudo".into(),
    };
    send_async(&mut a, &offer).await.unwrap();
    let decoded: Offer = receive_async(&mut b).await.unwrap();
    assert_eq!(decoded.ticket, offer.ticket);
    assert_eq!(decoded.socket, offer.socket);
}
