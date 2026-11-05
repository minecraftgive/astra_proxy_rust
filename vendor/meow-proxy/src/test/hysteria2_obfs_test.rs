use super::*;

#[test]
fn hop_ports_expansion_bounded() {
    // Repeated full ranges would pre-dedup into a giant Vec — the port
    // space can't hold more than u16::MAX entries anyway (issue #648).
    assert!(HopPorts::parse("1-65535").unwrap().is_some());
    assert!(HopPorts::parse("1-65535,1-65535").is_err());
    assert!(HopPorts::parse("1-65535,80").is_err());
    assert!(HopPorts::parse("80,443,1-65535").is_err());
    // A full range plus duplicates *within* the bound stays fine.
    let p = HopPorts::parse("1-1024,80,443").unwrap().unwrap();
    assert!(p.contains(80));
    assert!(!p.contains(2000));
}

#[test]
fn salamander_round_trip() {
    let obfs = Salamander::new(b"secret");
    let encoded = obfs.encode(b"payload");
    assert_ne!(&encoded[SALAMANDER_SALT_LEN..], b"payload");
    assert_eq!(obfs.decode(&encoded).unwrap(), b"payload");
}

#[test]
fn hop_state_clamps_absurd_interval() {
    // Programmatic callers skip the config-parse bound — the backstop
    // keeps `Instant + Duration` from overflowing (issue #648).
    let state = HopState::new("127.0.0.1:443".parse().unwrap(), "80", u64::MAX, u64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(
        state.min,
        Duration::from_secs(meow_common::MAX_DURATION_SECS)
    );
    assert_eq!(
        state.max,
        Duration::from_secs(meow_common::MAX_DURATION_SECS)
    );
}

#[test]
fn hop_ports_parse_ranges() {
    let ports = HopPorts::parse("443,8443-8445").unwrap().unwrap();
    assert!(ports.contains(443));
    assert!(ports.contains(8443));
    assert!(ports.contains(8445));
    assert!(!ports.contains(8446));
}
