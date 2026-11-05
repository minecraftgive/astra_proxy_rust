use super::*;
#[test]
fn monitor_cpu_identity_and_monotonic_deltas() {
    let first = ProcessIdentity {
        pid: 1,
        started: "first".into(),
    };
    let next = ProcessIdentity {
        pid: 1,
        started: "next".into(),
    };
    assert_eq!(
        cpu_delta(&first, &first, 2.0, 5.0, Duration::from_secs(2)),
        Some(150.0)
    );
    assert_eq!(
        cpu_delta(&first, &next, 2.0, 5.0, Duration::from_secs(2)),
        None
    );
    assert_eq!(
        cpu_delta(&first, &first, 5.0, 2.0, Duration::from_secs(2)),
        None
    );
    assert_eq!(cpu_delta(&first, &first, 2.0, 5.0, Duration::ZERO), None);
}
#[test]
fn monitor_linux_stat_handles_parentheses_and_units() {
    let mut fields = vec!["0"; 22];
    fields[0] = "R";
    fields[11] = "250";
    fields[12] = "50";
    fields[20] = "999999";
    fields[21] = "12";
    let text = format!("42 (a process ) with parentheses) {}", fields.join(" "));
    let raw = parse_linux(&text, 100, 4096).unwrap();
    assert_eq!(raw.cpu_seconds, 3.0);
    assert_eq!(raw.memory_bytes, 49152);
    assert_eq!(raw.virtual_memory_bytes, Some(999999));
    assert!(parse_linux("bad stat", 100, 4096).is_err());
}
#[test]
fn monitor_current_process_is_real_and_first_cpu_is_unknown() {
    let mut sampler = Sampler::default();
    let first = sampler.sample(std::process::id()).unwrap();
    assert_eq!(first.identity.pid, std::process::id());
    assert_eq!(first.cpu_percent, None);
    assert!(first.memory_bytes > 0);
    std::thread::sleep(Duration::from_millis(270));
    let second = sampler.sample(std::process::id()).unwrap();
    assert!(second
        .cpu_percent
        .is_some_and(|n| n >= 0.0 && n.is_finite()));
}
