// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Access plans: correctness at 4x oversubscription, optimal eviction on
//! cyclic and non-sequential plans, safe replacement under load.

use std::path::PathBuf;
use std::sync::Arc;

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

fn cfg(size: usize, budget: usize) -> Config {
    let mut c = Config::new(size as u64, budget, tmp());
    c.block_size = bs();
    c.policy = Policy::Clock;
    c
}

/// Fill the whole region with `pattern` through the transparent view and
/// write it back, so every block is on the SSD and clean.
fn fill(r: &Region) {
    let p = r.as_ptr() as *mut u64;
    for i in 0..r.len() / 8 {
        unsafe { p.add(i).write_volatile(pattern(i)) };
    }
    r.flush().unwrap();
}

/// Read every 8th word of `[off, off+len)`, verifying it.
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

/// A fixed pseudo-random permutation of `n` chunks.
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

#[test]
fn plan_builder_merges_and_persists() {
    let mut p = Plan::cyclic();
    p.push(0, 100).push(100, 50).push(400, 10).push(0, 0);
    assert_eq!(p.ranges(), &[(0, 150), (400, 10)]);
    assert_eq!(p.total_bytes(), 160);
    p.set_lookahead(12345);
    let path = tmp().join("plan-test.ddram-plan");
    p.save(&path).unwrap();
    let q = Plan::load(&path).unwrap();
    assert_eq!(p, q);
    assert!(q.is_cyclic());
    assert_eq!(q.lookahead(), 12345);
    std::fs::remove_file(&path).unwrap();
    assert!(Plan::load(tmp().join("missing.plan")).is_err());
    let f = Plan::from_blocks([3u64, 4, 5, 9, 10], 4096, false);
    assert_eq!(f.ranges(), &[(12288, 12288), (36864, 8192)]);
    assert!(!f.is_cyclic());
}

/// Bytes read by `passes` full sweeps of `r` (verifying every value).
fn sweep_bytes(r: &Region, passes: usize) -> u64 {
    let before = r.stats().bytes_read;
    for _ in 0..passes {
        assert_eq!(verify(r, 0, r.len()), 0);
    }
    r.stats().bytes_read - before
}

#[test]
fn cyclic_plan_is_scan_resistant_at_4x() {
    // 64 MiB swept in 16 MiB: plain CLOCK re-reads everything every pass;
    // the plan keeps a stable prefix (Belady) and streams the rest, which
    // must match the hand-tuned cyclic policy on the one pattern that policy
    // was built for.
    let passes = 4;
    let mut c = cfg(64 * MB, 16 * MB);
    c.policy = Policy::Cyclic;
    c.prefetch_depth = 4;
    let r = Region::new(c).unwrap();
    fill(&r);
    let cyclic = sweep_bytes(&r, passes);
    drop(r);

    let r = Region::new(cfg(64 * MB, 16 * MB)).unwrap();
    fill(&r);
    let mut plan = Plan::cyclic();
    plan.push(0, r.len() as u64);
    r.set_plan(&plan).unwrap();
    assert!(r.stats().plan_active);
    let read = sweep_bytes(&r, passes);
    let s = r.stats();
    let lru = (passes * r.len()) as u64;
    assert!(read < lru * 9 / 10, "plan read {read} vs LRU {lru}: {s:?}");
    assert!(
        read <= cyclic + cyclic / 10,
        "plan read {read} vs cyclic policy {cyclic}: {s:?}"
    );
    assert!(s.plan_hits > 0 && s.plan_misses == 0, "{s:?}");
    assert!(s.plan_position > 0, "{s:?}");
}

#[test]
fn shuffled_plan_beats_heuristics() {
    // Chunks accessed in a fixed pseudo-random order, repeatedly: no stream
    // detector can prefetch this and no recency policy can keep the right
    // subset. With the order declared, reads drop well below LRU.
    let chunk = 4 * bs();
    let size = 64 * MB;
    let r = Region::new(cfg(size, 16 * MB)).unwrap();
    fill(&r);
    let order = shuffled(size / chunk, 7);
    let mut plan = Plan::cyclic();
    for &c in &order {
        plan.push((c * chunk) as u64, chunk as u64);
    }
    r.set_plan(&plan).unwrap();
    let before = r.stats().bytes_read;
    let passes = 4;
    let mut bad = 0;
    for _ in 0..passes {
        for &c in &order {
            bad += verify(&r, c * chunk, chunk);
        }
    }
    assert_eq!(bad, 0);
    let s = r.stats();
    let read = s.bytes_read - before;
    let lru = (passes * size) as u64;
    assert!(read < lru * 8 / 10, "plan read {read} vs LRU {lru}: {s:?}");
    assert_eq!(s.plan_misses, 0, "{s:?}");
}

#[test]
fn linear_plan_with_hot_range_is_retained() {
    // A hot range is revisited between cold ranges the plan touches once.
    // Belady keeps the hot range resident and evicts each cold range right
    // after use; total reads must stay close to the cold data plus one load
    // of the hot range.
    let size = 64 * MB;
    let hot = 2 * MB;
    let cold = 2 * MB;
    let r = Region::new(cfg(size, 12 * MB)).unwrap();
    fill(&r);
    let mut plan = Plan::new();
    let mut off = hot;
    while off + cold <= size {
        plan.push(0, hot as u64).push(off as u64, cold as u64);
        off += cold;
    }
    r.set_plan(&plan).unwrap();
    let before = r.stats().bytes_read;
    let mut bad = 0;
    let mut off = hot;
    while off + cold <= size {
        bad += verify(&r, 0, hot);
        bad += verify(&r, off, cold);
        off += cold;
    }
    assert_eq!(bad, 0);
    let s = r.stats();
    let read = s.bytes_read - before;
    // Everything once, plus slack for prefetch past the end of the plan.
    assert!(read <= (size + 4 * MB) as u64, "read {read}: {s:?}");
    assert_eq!(s.plan_misses, 0, "{s:?}");
}

#[test]
fn plan_concurrent_consumers_verify() {
    // Four threads consume each range concurrently, as a multi-threaded
    // matrix-vector product does with a layer, at 4x oversubscription.
    let size = 64 * MB;
    let range = 2 * MB;
    let r = Arc::new(Region::new(cfg(size, 16 * MB)).unwrap());
    fill(&r);
    let mut plan = Plan::cyclic();
    for c in shuffled(size / range, 3) {
        plan.push((c * range) as u64, range as u64);
    }
    r.set_plan(&plan).unwrap();
    let ranges: Vec<(u64, u64)> = plan.ranges().to_vec();
    for _ in 0..3 {
        for &(off, len) in &ranges {
            let hs: Vec<_> = (0..4)
                .map(|t| {
                    let r = r.clone();
                    std::thread::spawn(move || {
                        let part = len as usize / 4;
                        verify(&r, off as usize + t * part, part)
                    })
                })
                .collect();
            let bad: u64 = hs.into_iter().map(|h| h.join().unwrap()).sum();
            assert_eq!(bad, 0);
        }
    }
    let s = r.stats();
    assert_eq!(s.plan_misses, 0, "{s:?}");
    assert!(s.bytes_read < (4 * size) as u64, "{s:?}");
}

#[test]
fn plan_replacement_under_load() {
    // Readers hammer the region while plans are installed, replaced and
    // removed. Data must verify and nothing may crash or hang.
    let size = 32 * MB;
    let r = Arc::new(Region::new(cfg(size, 8 * MB)).unwrap());
    fill(&r);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|t| {
            let r = r.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let p = r.as_ptr() as *const u64;
                let words = r.len() / 8;
                let mut x = 0x5555u64 + t;
                let mut bad = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let i = x as usize % words;
                    if unsafe { p.add(i).read_volatile() } != pattern(i) {
                        bad += 1;
                    }
                }
                bad
            })
        })
        .collect();
    for k in 0..40 {
        let mut plan = Plan::cyclic();
        let chunk = (k % 7 + 1) * bs();
        for c in shuffled(size / chunk, k as u64) {
            plan.push((c * chunk) as u64, chunk as u64);
        }
        r.set_plan(&plan).unwrap();
        if k % 5 == 4 {
            r.clear_plan();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let bad: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(bad, 0);
    r.clear_plan();
    assert!(!r.stats().plan_active);
    // Back on CLOCK, everything still verifies.
    assert_eq!(verify(&r, 0, r.len()), 0);
}

#[test]
fn plan_with_writes_and_persist() {
    // Dirty blocks evicted by the plan policy are written back and read
    // back correctly; the plan survives a write-heavy pass.
    let r = Region::new(cfg(32 * MB, 8 * MB)).unwrap();
    let mut plan = Plan::cyclic();
    let chunk = MB;
    for c in shuffled(32, 11) {
        plan.push((c * chunk) as u64, chunk as u64);
    }
    r.set_plan(&plan).unwrap();
    let p = r.as_ptr() as *mut u64;
    for &(off, len) in plan.ranges() {
        for i in off as usize / 8..(off + len) as usize / 8 {
            unsafe { p.add(i).write_volatile(pattern(i)) };
        }
    }
    let mut bad = 0;
    for _ in 0..2 {
        for &(off, len) in plan.ranges() {
            bad += verify(&r, off as usize, len as usize);
        }
    }
    assert_eq!(bad, 0);
    let s = r.stats();
    assert!(s.writebacks > 0, "{s:?}");
    assert_eq!(s.plan_misses, 0, "{s:?}");
}

#[test]
fn invalid_plans_are_rejected() {
    let r = Region::new(cfg(16 * MB, 4 * MB)).unwrap();
    assert!(r.set_plan(&Plan::new()).is_err());
    let mut out = Plan::new();
    out.push(r.len() as u64 + MB as u64, MB as u64);
    assert!(r.set_plan(&out).is_err());
    assert!(!r.stats().plan_active);
}
