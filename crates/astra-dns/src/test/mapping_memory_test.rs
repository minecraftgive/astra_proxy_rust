use super::*;
use crate::maintenance::Maintain;
#[test]
fn renewals_keep_single_expiry_and_shared_domain_and_reclaim_after_twenty_cycles() {
    let config = Config {
        fake_ip: true,
        fake_ip_range6: Some("fdab::/64".into()),
        ..Default::default()
    };
    let mut state = SharedMappings::default();
    for _ in 0..20 {
        let ip = state
            .mappings
            .allocate(&config, "shared.test", false, &HashSet::new())
            .unwrap()
            .unwrap();
        for _ in 0..200 {
            assert_eq!(
                state
                    .mappings
                    .allocate(&config, "shared.test", false, &HashSet::new())
                    .unwrap(),
                Some(ip)
            );
            state
                .mappings
                .observe(&config, "shared.test", "203.0.113.1".parse().unwrap(), 60);
        }
        assert_eq!(state.mappings.fake_expiry.len(), 1);
        assert_eq!(state.mappings.real_expiry.len(), 1);
        let (domain, _) = state.mappings.domains[0]
            .get_key_value("shared.test")
            .unwrap();
        assert!(Arc::ptr_eq(domain, &state.mappings.fake[&ip].domain));
        let v6 = state
            .mappings
            .allocate(&config, "shared.test", true, &HashSet::new())
            .unwrap()
            .unwrap();
        assert!(v6.is_ipv6());
        let future = Instant::now() + Duration::from_secs(u64::from(config.mapping_ttl_secs) + 61);
        state.maintain(future);
        state.maintain(future + Duration::from_secs(31));
        assert!(state.mappings.fake.is_empty() && state.mappings.real.is_empty());
        assert!(state.mappings.fake_expiry.is_empty() && state.mappings.real_expiry.is_empty());
        assert_eq!(state.mappings.fake.capacity(), 0);
        assert!(state
            .mappings
            .domains
            .iter()
            .all(|m| m.is_empty() && m.capacity() == 0));
    }
}
