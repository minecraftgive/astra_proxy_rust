use super::*;

#[test]
fn round_trip_via_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sel.json");
    let s = SelectorStore::open(path.clone());
    assert!(s.get("g").is_none());
    s.set("g", "node-a");
    assert_eq!(s.get("g").as_deref(), Some("node-a"));

    // Reopen, value survives.
    drop(s);
    let s2 = SelectorStore::open(path);
    assert_eq!(s2.get("g").as_deref(), Some("node-a"));
}

#[test]
fn missing_file_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let s = SelectorStore::open(dir.path().join("does-not-exist.json"));
    assert!(s.get("anything").is_none());
}

#[test]
fn malformed_file_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, b"{ not json").unwrap();
    let s = SelectorStore::open(path);
    assert!(s.get("anything").is_none());
    // And it recovers — subsequent set() rewrites the file cleanly.
    s.set("g", "x");
    assert_eq!(s.get("g").as_deref(), Some("x"));
}

#[test]
fn concurrent_sets_converge_to_latest_map_on_disk() {
    // Issue #621: writers used to snapshot+rename independently, so a
    // slow writer could land an older map last. With the serialized
    // write lock the file must always end up equal to the final map.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sel.json");
    let s = SelectorStore::open(path.clone());
    let mut handles = Vec::new();
    for t in 0..8 {
        let s = Arc::clone(&s);
        handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                s.set(&format!("g{}", i % 4), &format!("t{t}-v{i}"));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let on_disk: HashMap<String, String> =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let in_memory = s.map.lock().clone();
    assert_eq!(on_disk, in_memory);
}

#[test]
fn failed_write_retries_on_next_set_even_with_unchanged_value() {
    // Issue #621 review: a failed write_atomic must leave `dirty`
    // asserted, or a later set() carrying the *same* value would
    // early-return and the disk would stay stale forever.
    let dir = tempfile::tempdir().unwrap();
    // The store's parent path is a regular FILE — create_dir_all
    // fails, so every write_atomic errors deterministically.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    let path = blocker.join("sel.json");
    let s = SelectorStore::open(path.clone());
    s.set("g", "node-a");
    assert!(!path.exists(), "write through a file-parent must fail");

    // Repair the parent path: the same-value set must still persist
    // because the failed write left the store dirty.
    std::fs::remove_file(&blocker).unwrap();
    s.set("g", "node-a");
    let on_disk: HashMap<String, String> = serde_json::from_slice(&std::fs::read(&path).unwrap())
        .expect("dirty flag must force a rewrite despite an unchanged value");
    assert_eq!(on_disk.get("g").map(String::as_str), Some("node-a"));
}
