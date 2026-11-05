use super::*;

// The only test that sets the flag: it is process-global and one-way,
// and the dead-off-platform assertions in meow-rules / meow-tunnel run
// in other binaries.
#[test]
fn inbound_process_info_keeps_process_and_uid_rules_live() {
    enable_inbound_process_info();
    assert!(process_info_available());
    assert!(uid_info_available());
}
