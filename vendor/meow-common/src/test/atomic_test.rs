use super::*;
use std::sync::atomic::Ordering;

#[test]
fn checked_increment_pre_increments() {
    let counter = AtomicU::new(0);
    assert_eq!(checked_increment(&counter), Some(1));
    assert_eq!(checked_increment(&counter), Some(2));
}

#[test]
fn checked_increment_never_emits_the_sentinel() {
    let counter = AtomicU::new(Uint::MAX - 2);
    #[allow(
        clippy::useless_conversion,
        reason = "identity on 64-bit; u32→u64 widening on mips32"
    )]
    let last_legit = u64::from(Uint::MAX) - 1;
    assert_eq!(checked_increment(&counter), Some(last_legit));
    assert_eq!(
        checked_increment(&counter),
        None,
        "Uint::MAX is the window's always-reject sentinel"
    );
    assert_eq!(
        counter.load(Ordering::Relaxed),
        Uint::MAX - 1,
        "a refused increment leaves the counter terminal, not wrapped"
    );
    assert_eq!(checked_increment(&counter), None);
}
