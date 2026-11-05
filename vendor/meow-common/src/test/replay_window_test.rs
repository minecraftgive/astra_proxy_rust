use super::*;

#[test]
fn sequential_ids_accept_once() {
    let mut w = ReplayWindow::new();
    assert!(w.check_and_set(0));
    assert!(w.check_and_set(1));
    assert!(w.check_and_set(2));
    assert!(!w.check_and_set(0), "replay of an old ID is dropped");
    assert!(!w.check_and_set(1));
    assert!(!w.check_and_set(2));
}

#[test]
fn out_of_order_within_window_accepted() {
    let mut w = ReplayWindow::new();
    assert!(w.check_and_set(100));
    assert!(w.check_and_set(50), "in-window reorder is fine");
    assert!(!w.check_and_set(50));
    assert!(!w.check_and_set(100));
    assert!(w.check_and_set(0), "still inside the trailing window");
}

#[test]
fn ids_older_than_window_rejected() {
    let mut w = ReplayWindow::new();
    assert!(w.check_and_set(0));
    assert!(w.check_and_set(WINDOW_BITS));
    assert!(!w.check_and_set(0), "pre-window ID is too old");
    assert!(w.check_and_set(1), "still inside the trailing window");
}

#[test]
fn huge_jump_resets_window() {
    let mut w = ReplayWindow::new();
    assert!(w.check_and_set(7));
    assert!(w.check_and_set(u64::MAX - 3), "jump past the window width");
    assert!(
        !w.check_and_set(7),
        "everything before the jump is pre-window"
    );
    assert!(!w.check_and_set(u64::MAX - 3));
    assert!(!w.check_and_set(u64::MAX), "u64::MAX is never accepted");
}

#[test]
fn jump_clears_stale_bits() {
    let mut w = ReplayWindow::new();
    assert!(w.check_and_set(63), "records position 0 (max_seen = 63)");
    assert!(w.check_and_set(0), "records position 63 (bits[0] bit 63)");
    // A shift of exactly one window width must clear every recorded bit.
    assert!(w.check_and_set(63 + WINDOW_BITS));
    assert!(
        w.check_and_set(63 + WINDOW_BITS - 63),
        "position 63 must not carry the stale bit into the new window"
    );
}

#[test]
fn limb_aligned_shift() {
    // The rem == 0 shift path (by a multiple of 64) is a pure limb copy.
    let mut w = ReplayWindow::new();
    assert!(w.check_and_set(5));
    assert!(w.check_and_set(5 + 64));
    assert!(!w.check_and_set(5), "the recorded bit moved a whole limb");
    assert!(w.check_and_set(4), "in-window reorder still tracked");
}

#[test]
fn maximal_non_clearing_shift() {
    // by = WINDOW_BITS - 1: limbs = 31, rem = 63 — the deepest carry.
    let mut w = ReplayWindow::new();
    assert!(w.check_and_set(0));
    assert!(w.check_and_set(WINDOW_BITS - 1));
    assert!(
        !w.check_and_set(0),
        "id 0 must sit at the window's trailing edge"
    );
    assert!(!w.check_and_set(WINDOW_BITS - 1));
}

#[test]
fn partial_limb_shifts_preserve_bits() {
    let mut w = ReplayWindow::new();
    // Non-monotonic acceptance straddling limb boundaries.
    for id in [100, 63, 64, 200, 65, 130] {
        assert!(w.check_and_set(id), "id {id}");
    }
    for id in [100, 63, 64, 200, 65, 130] {
        assert!(!w.check_and_set(id), "replayed id {id}");
    }
    // A small jump keeps old IDs inside the window — still recorded.
    assert!(w.check_and_set(330));
    assert!(!w.check_and_set(130), "its bit must have shifted intact");
    assert!(!w.check_and_set(100));
    assert!(w.check_and_set(229), "fresh in-window ID");
}
