use super::*;
#[test]
fn sources_are_separate_and_stale_or_invalid_caches_are_ignored() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "astra-subscription-view-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("proxy.toml");
    let base="[subscriptions.a]\nurl='https://example.com/a'\n[subscriptions.b]\nurl='https://example.com/b'\n[subscription]\nurl='https://example.com/legacy'\nmanaged_nodes=['old']\n";
    std::fs::write(&path, base).unwrap();
    let cache = serde_json::json!({"base":base,"effective":"[[outbounds]]\ntag='one'\npassword='credential'\n[[outbounds]]\ntag='two'\n","subscriptions":{"a":{"url":"https://example.com/a","managed_nodes":["one","missing","one"]},"b":{"url":"https://example.com/b","managed_nodes":["two"]}},"ownership":{"url":"https://example.com/legacy","managed_nodes":["one"]}});
    let cache_path = path.with_extension("subscription-cache.json");
    std::fs::write(&cache_path, cache.to_string()).unwrap();
    let views = read(&path).unwrap();
    assert_eq!(views["a"].tags, ["one"]);
    assert_eq!(views["b"].tags, ["two"]);
    assert_eq!(views["__legacy__"].tags, ["one"]);
    assert!(!serde_json::to_string(&views)
        .unwrap()
        .contains("credential"));
    std::fs::write(&path, format!("{base}\n#changed")).unwrap();
    assert!(read(&path).unwrap()["a"].tags.is_empty());
    std::fs::write(&cache_path, "invalid").unwrap();
    assert_eq!(read(&path).unwrap()["__legacy__"].tags, ["old"]);
    std::fs::remove_file(cache_path).unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
