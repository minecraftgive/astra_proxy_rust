use super::*;
use sha2::{Digest, Sha256};

fn profiles() -> impl Iterator<Item = ShapeProfile> {
    (0..400).map(|i| ShapeProfile::new(format!("meow-shape-{i}").as_bytes()))
}

#[test]
fn every_style_interleave_and_budget_is_reachable() {
    let (mut styles, mut interleaves, mut budgets, mut jitter) =
        ([false; 4], [false; 3], [false; 3], [false; 2]);
    for p in profiles() {
        styles[match p.style {
            FillStyle::Popcount { .. } => 0,
            FillStyle::Text { .. } => 1,
            FillStyle::Digits { .. } => 2,
            FillStyle::Motif { .. } => 3,
        }] = true;
        interleaves[match p.interleave {
            Interleave::Strided { .. } => 0,
            Interleave::Blocks { .. } => 1,
            Interleave::Keyed { .. } => 2,
        }] = true;
        budgets[match p.budget {
            Budget::Ramp => 0,
            Budget::Buckets(_) => 1,
            Budget::Jitter(_) => 2,
        }] = true;
        jitter[usize::from(p.target_jitter.is_some())] = true;
    }
    assert!(styles
        .iter()
        .chain(&interleaves)
        .chain(&budgets)
        .chain(&jitter)
        .all(|&x| x));
}

#[test]
fn salt_block_round_trips_and_slots_are_distinct() {
    let salt: [u8; SALT_LEN] = std::array::from_fn(|i| (i * 17 + 3) as u8);
    for p in profiles() {
        let mut block = vec![0u8; p.salt_block_len()];
        p.write_salt_block(&salt, &mut block);
        assert_eq!(p.read_salt_block(&block), salt);
        let mut seen = [false; 256];
        for (pos, _) in p.salt_slots {
            assert!(usize::from(pos) < block.len());
            assert!(!std::mem::replace(&mut seen[usize::from(pos)], true));
        }
    }
}

#[test]
fn interleave_is_an_involution() {
    for p in profiles().take(60) {
        for (pad, sealed) in [(0, 20), (5, 3), (90, 70), (400, 1200), (1500, 40)] {
            let a0: Vec<u8> = (0..pad).map(|i| i as u8).collect();
            let b0: Vec<u8> = (0..sealed).map(|i| (i * 7 + 1) as u8).collect();
            let (mut a, mut b) = (a0.clone(), b0.clone());
            p.interleave(9, &mut a, &mut b);
            p.interleave(9, &mut a, &mut b);
            assert_eq!((a, b), (a0, b0));
        }
    }
}

#[test]
fn filler_bytes_follow_the_profile_style() {
    for p in profiles() {
        let mut buf = [0u8; 300];
        p.fill(7, &mut buf);
        match p.style {
            FillStyle::Popcount { .. } => {
                let ones = buf[0].count_ones();
                assert!((2..=6).contains(&ones));
                assert!(buf.iter().all(|b| b.count_ones() == ones));
            }
            FillStyle::Text { .. } => {
                assert!(buf.iter().all(|&b| (0x20..=0x7e).contains(&b) || b >= 0x80));
            }
            FillStyle::Digits { .. } => {
                assert!(buf.iter().all(|&b| b & 0x0f < 10));
            }
            FillStyle::Motif { period, .. } => {
                for (i, &b) in buf.iter().enumerate() {
                    let phase = i % period;
                    if phase + 3 >= period && phase + 1 < period {
                        assert!(b.is_ascii_digit());
                    }
                }
            }
        }
    }
}

#[test]
fn budget_ramps_then_restarts_after_idle() {
    for p in profiles().take(60) {
        let mut state = ShapeState::default();
        let first = p.next_budget(&mut state, 1_000);
        assert!((0x40..=p.budget_first).contains(&first));
        for seq in 1..40 {
            state.seq = seq;
            let budget = p.next_budget(&mut state, 1_000);
            assert!((0x40..=p.budget_max).contains(&budget));
        }
        assert_eq!(state.chunk, p.budget_max);
        state.seq = 40;
        p.next_budget(&mut state, 1_000 + p.idle_reset_secs + 1);
        assert_eq!(
            state.chunk,
            (p.budget_initial + p.budget_step).min(p.budget_max)
        );
    }
}

#[test]
fn padding_stays_in_bounds() {
    for p in profiles().take(100) {
        for seq in 0..64 {
            for payload in [0, 1, 100, 700, 1400, 5000, RECORD_LEN_MAX] {
                let prefix = p.prefix_len(seq);
                assert!((p.prefix_lo..=p.prefix_hi).contains(&prefix));
                let padding = p.padding_len(seq, payload, prefix, seq == 0);
                assert!(padding <= p.pad_hi + 2 * TOP_UP_MAX);
            }
        }
    }
}

/// Feed everything `p` decides into `h`.
fn fingerprint(p: &ShapeProfile, h: &mut Sha256) {
    let mut put = |v: usize| h.update((v as u32).to_le_bytes());
    put(p.salt_block_len());
    for seq in 0..48u32 {
        let prefix = p.prefix_len(seq);
        put(prefix);
        for payload in [0, 1, 99, 300, 1000, 1400, 5000] {
            put(p.padding_len(seq, payload, prefix, false));
            put(p.padding_len(seq, payload, prefix, true));
        }
    }
    let mut state = ShapeState::default();
    for (seq, now) in (0..40u32).zip([5, 5, 5, 6, 9, 200, 200, 201].into_iter().cycle()) {
        state.seq = seq;
        put(p.next_budget(&mut state, 1_000_000 + now));
    }
    for seq in [0, 1, 2, 77, SALT_BLOCK_SEQ] {
        let mut fill = [0u8; 80];
        p.fill(seq, &mut fill);
        h.update(fill);
    }
    let mut block = vec![0u8; p.salt_block_len()];
    p.write_salt_block(&std::array::from_fn(|i| 0xa0 + i as u8), &mut block);
    h.update(&block);
    let mut pad: Vec<u8> = (0..90).map(|i| (i * 3 + 1) as u8).collect();
    let mut sealed: Vec<u8> = (0..70).map(|i| (i * 5 + 2) as u8).collect();
    p.interleave(4, &mut pad, &mut sealed);
    h.update(&pad);
    h.update(&sealed);
}

/// Regression pin over the same PSKs that reach every profile variant.
/// All of these values are part of the wire protocol, so an unintended
/// change must fail loudly; the pin was recorded after the profile
/// interoperated with a real snell-server v6 in `default` mode.
#[test]
fn profile_fingerprint_is_stable() {
    let mut h = Sha256::new();
    for p in profiles() {
        fingerprint(&p, &mut h);
    }
    assert_eq!(
        hex::encode(h.finalize()),
        "74775765529397f99a20650432f5918f9d8c2d8500f2006bceaf7154a48df67c"
    );
}
