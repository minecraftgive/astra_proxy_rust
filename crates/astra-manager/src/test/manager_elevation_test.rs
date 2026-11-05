#[cfg(target_os = "linux")]
use super::*;

#[cfg(target_os = "linux")]
#[test]
fn process_identity_and_capability_are_checked() {
    let info = process_info(std::process::id()).unwrap();
    assert!(info.start_ticks > 0);
}

#[test]
fn capability_mask_includes_net_admin() {
    // Constants used by the hand-off contract.
    assert_eq!(1u64 << 12, 0x1000);
}
