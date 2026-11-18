use super::*;

#[test]
fn borrowed_route_key_matches_owned_hash_and_all_route_context() {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let key = CacheKey {
        destination_ip: Some("203.0.113.1".parse().unwrap()),
        target: astra_model::Target {
            host: "example.org".into(),
            port: 443,
        },
        revision: 9,
        network: astra_model::Network::Tcp,
        inbound: Inbound::Socks5,
        process: None,
    };
    let borrowed = CacheKeyRef {
        destination_ip: key.destination_ip,
        target: &key.target,
        revision: key.revision,
        network: key.network,
        inbound: key.inbound,
        process: key.process.as_ref(),
    };
    let mut owned_hash = DefaultHasher::new();
    let mut borrowed_hash = DefaultHasher::new();
    key.hash(&mut owned_hash);
    borrowed.hash(&mut borrowed_hash);
    assert_eq!(owned_hash.finish(), borrowed_hash.finish());
    let cache = moka::sync::Cache::new(2);
    cache.insert(key.clone(), astra_model::RouteAction::Direct);
    assert_eq!(cache.get(&borrowed), Some(astra_model::RouteAction::Direct));
    assert!(cache
        .get(&CacheKeyRef {
            revision: 10,
            ..borrowed
        })
        .is_none());
}
