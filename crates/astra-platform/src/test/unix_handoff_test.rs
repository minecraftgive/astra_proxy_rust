use super::*;

#[test]
fn installation_trust_permissions_cover_owner_and_writers() {
    for (uid, gid, mode, trusted) in [
        (0, 0, 0o755, true),
        (0, 0, 0o775, true),
        (0, 1000, 0o755, true),
        (0, 1000, 0o775, false),
        (0, 0, 0o777, false),
        (0, 0, 0o1777, false),
        (1000, 0, 0o755, false),
        (1000, 1000, 0o444, false),
    ] {
        assert_eq!(
            root_controlled(uid, gid, mode),
            trusted,
            "uid={uid} gid={gid} mode={mode:o}"
        );
    }
}

#[test]
fn writable_installation_warns_and_resolves_the_program() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("astra-proxy");
    std::fs::write(&program, b"program").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o777)).unwrap();
    let alias = directory.path().join("proxy-link");
    symlink(&program, &alias).unwrap();

    let inspected = inspect_program(&alias).unwrap();
    assert_eq!(inspected.path, std::fs::canonicalize(&program).unwrap());
    let warning = inspected.warning.unwrap();
    assert!(warning.contains(&inspected.path.display().to_string()));
    assert!(warning.contains("continuing TUN elevation"));
    assert_eq!(
        ensure_root_controlled(&alias).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn writable_ancestor_is_advisory_for_root_owned_programs() {
    use std::os::unix::fs::PermissionsExt;

    // A non-root test process cannot create the trusted leaf needed to isolate
    // its ancestor. Ownership cases are also covered by the permission table.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let program = directory.path().join("astra-proxy");
    std::fs::write(&program, b"program").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

    let inspected = inspect_program(&program).unwrap();
    assert!(inspected.warning.unwrap().contains(
        &std::fs::canonicalize(directory.path())
            .unwrap()
            .display()
            .to_string()
    ));
    assert!(ensure_root_controlled(&program).is_err());
}

#[test]
fn unavailable_installation_metadata_is_a_warning() {
    let warning = program_warning(
        std::path::Path::new("/installation"),
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "inspection denied",
        )),
    )
    .unwrap();
    assert!(warning.contains("/installation"));
    assert!(warning.contains("inspection denied"));
    assert!(warning.contains("/usr/local/bin"));
    assert!(warning.contains("continuing TUN elevation"));
}

#[test]
fn invalid_program_paths_still_fail() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    assert_eq!(
        inspect_program(&missing).err().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        inspect_program(directory.path()).err().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(ensure_root_controlled(&missing).is_err());
    assert!(ensure_root_controlled(directory.path()).is_err());
}

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
