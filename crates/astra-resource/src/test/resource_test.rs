use super::*;
use std::{io::Write, net::TcpListener};

#[test]
fn read_only_prefer_cache_and_refresh_policies_are_independent() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    fs::write(&source, b"41").unwrap();
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/resource", server.local_addr().unwrap());
    let decode = |bytes: &[u8]| -> Result<u32> { Ok(std::str::from_utf8(bytes)?.parse()?) };
    let readonly = ResourceLoader::new(LoadPolicy::ReadOnly);
    let cached = ResourceLoader::new(LoadPolicy::PreferCache);
    assert_eq!(
        readonly
            .load(directory.path(), "source", Some(&url), 0, 100, decode)
            .unwrap()
            .value,
        41
    );
    let stale = cached
        .load(directory.path(), "source", Some(&url), 0, 100, decode)
        .unwrap();
    assert_eq!(stale.cache_status, CacheStatus::Stale);
    assert!(cached.served_stale());
    assert!(!readonly.served_stale());
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = server.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = [0; 1024];
        let received = stream.read(&mut request).unwrap();
        assert!(received > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n42")
            .unwrap();
    });
    let refreshed = ResourceLoader::default()
        .load(directory.path(), "source", Some(&url), 0, 100, decode)
        .unwrap();
    worker.join().unwrap();
    assert_eq!(
        (refreshed.value, refreshed.cache_status),
        (42, CacheStatus::Refreshed)
    );
    assert_eq!(fs::read(&source).unwrap(), b"41");
    let runtime = astra_platform::storage::runtime_path(&source, "resources").unwrap();
    assert_eq!(fs::read(&runtime).unwrap(), b"42");
    fs::remove_file(runtime).unwrap();
}
