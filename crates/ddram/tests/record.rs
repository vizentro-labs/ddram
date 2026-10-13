// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Learned plans: record one pass, replay it as a plan, persist it.

use std::path::PathBuf;

use ddram::{Config, Plan, Policy, Region};

const MB: usize = 1 << 20;

fn tmp() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

fn bs() -> usize {
    ddram::min_block_size().max(64 * 1024)
}

fn pattern(i: usize) -> u64 {
    (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03
}

fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    let mut x = seed | 1;
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, x as usize % (i + 1));
    }
    v
}

fn verify(r: &Region, off: usize, len: usize) -> u64 {
    let p = r.as_ptr() as *const u64;
    let mut bad = 0;
    let mut i = off / 8;
    while i < (off + len) / 8 {
        if unsafe { p.add(i).read_volatile() } != pattern(i) {
            bad += 1;
        }
        i += 8;
    }
    bad
}

#[test]
fn record_replay_and_persist() {
    let size = 64 * MB;
    let chunk = 4 * bs();
    let path = tmp().join("record-test.bin");
    {
        let data: Vec<u8> = (0..size / 8)
            .flat_map(|i| pattern(i).to_le_bytes())
            .collect();
        std::fs::write(&path, &data).unwrap();
    }
    let open = || {
        let mut c = Config::read_only_file(&path, 16 * MB);
        c.block_size = bs();
        c.policy = Policy::Clock;
        Region::new(c).unwrap()
    };
    let order = shuffled(size / chunk, 5);
    let plan_file = tmp().join("record-test.ddram-plan");

    // Run 1: record the first pass, learn the plan, use it for the rest.
    let r = open();
    r.record(4 * size / bs());
    for &c in &order {
        assert_eq!(verify(&r, c * chunk, chunk), 0);
    }
    let learned = r.recorded_plan(true).expect("trace");
    let trace = r.stop_recording();
    assert!(r.recorded().is_none());
    assert!(trace.len() >= size / bs(), "trace {}", trace.len());
    // One pass covers every block exactly once, in the shuffled order.
    assert_eq!(learned.total_bytes(), size as u64);
    assert_eq!(learned.len(), order.len());
    assert_eq!(
        learned.ranges()[0],
        ((order[0] * chunk) as u64, chunk as u64)
    );
    r.set_plan(&learned).unwrap();
    let before = r.stats().bytes_read;
    for _ in 0..3 {
        for &c in &order {
            assert_eq!(verify(&r, c * chunk, chunk), 0);
        }
    }
    let with_plan = r.stats().bytes_read - before;
    assert!(with_plan < (3 * size * 8 / 10) as u64, "{with_plan}");
    assert_eq!(r.stats().plan_misses, 0);
    learned.save(&plan_file).unwrap();
    drop(r);

    // Run 2: warm start from the saved plan.
    let r = open();
    let saved = Plan::load(&plan_file).unwrap();
    assert_eq!(saved, learned);
    r.set_plan(&saved).unwrap();
    // The first pass of a fresh region reads everything once; the plan is
    // what makes the passes after it cheap.
    for &c in &order {
        assert_eq!(verify(&r, c * chunk, chunk), 0);
    }
    let before = r.stats().bytes_read;
    for _ in 0..3 {
        for &c in &order {
            assert_eq!(verify(&r, c * chunk, chunk), 0);
        }
    }
    let warm = r.stats().bytes_read - before;
    assert!(warm < (3 * size * 8 / 10) as u64, "{warm}");
    drop(r);
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&plan_file).unwrap();
}

#[test]
fn trace_is_cut_at_one_pass() {
    // A trace that ran into a second pass yields one cycle.
    let t = [5, 6, 7, 2, 3, 5, 6, 7, 2];
    assert_eq!(Plan::trace_cycle(&t), Some(5));
    let p = Plan::from_trace(&t, 4096, true);
    assert_eq!(p.ranges(), &[(5 * 4096, 3 * 4096), (2 * 4096, 2 * 4096)]);
    assert!(p.is_cyclic());
    // Nearby repeats (two threads faulting one block) are not a cycle.
    assert_eq!(Plan::trace_cycle(&[1, 1, 2, 2, 3, 4, 4, 5]), None);
    assert_eq!(
        Plan::from_trace(&[1, 1, 2], 4096, false).ranges(),
        &[(4096, 8192)]
    );
    // Recording with a small buffer keeps the first entries only.
    let r = Region::new({
        let mut c = Config::new((8 * MB) as u64, 4 * bs(), tmp());
        c.block_size = bs();
        c
    })
    .unwrap();
    r.record(3);
    r.write_at(0, &[1u8; 16]);
    r.write_at(r.len() - 16, &[1u8; 16]);
    r.write_at(r.len() / 2, &[1u8; 16]);
    r.write_at(bs(), &[1u8; 16]);
    let t = r.stop_recording();
    assert_eq!(t.len(), 3);
    assert_eq!(t[0], 0);
    assert_eq!(t[1], (r.len() / bs() - 1) as u64);
}
