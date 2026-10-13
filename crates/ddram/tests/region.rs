// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::Arc;

use ddram::{Advice, Config, Policy, Region};

const MB: usize = 1 << 20;

fn tmp() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

fn bs() -> usize {
    // 64 KiB covers Windows' allocation granularity and Apple Silicon pages.
    ddram::min_block_size().max(64 * 1024)
}

fn pattern(i: usize) -> u64 {
    (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03
}

fn cfg(size: usize, budget: usize) -> Config {
    let mut c = Config::new(size as u64, budget, tmp());
    c.block_size = bs();
    c
}

#[test]
fn transparent_4x_budget_roundtrip() {
    let r = Region::new(cfg(64 * MB, 16 * MB)).unwrap();
    let words = r.len() / 8;
    let p = r.as_ptr() as *mut u64;
    unsafe {
        for i in 0..words {
            *p.add(i) = pattern(i);
        }
        for i in 0..words {
            assert_eq!(*p.add(i), pattern(i), "word {i}");
        }
    }
    let s = r.stats();
    assert!(s.writebacks > 0, "data must have spilled to the SSD: {s:?}");
    assert!(s.resident_blocks as usize <= r.ram_budget() / r.block_size());
    // Second write pass over half the data, then verify everything again.
    unsafe {
        for i in (0..words).step_by(2) {
            *p.add(i) = !pattern(i);
        }
        for i in 0..words {
            let want = if i % 2 == 0 { !pattern(i) } else { pattern(i) };
            assert_eq!(*p.add(i), want, "word {i}");
        }
    }
}

#[test]
fn clean_blocks_are_never_written_back() {
    let r = Region::new(cfg(32 * MB, 8 * MB)).unwrap();
    let p = r.as_ptr();
    unsafe {
        for off in (0..r.len()).step_by(4096) {
            *p.add(off) = 1;
        }
    }
    let after_write = r.stats().writebacks;
    // Read-only sweeps evict only clean blocks...
    let mut sum = 0u64;
    for _ in 0..3 {
        unsafe {
            for off in (0..r.len()).step_by(4096) {
                sum += *p.add(off) as u64;
            }
        }
    }
    assert_eq!(sum, 3 * (r.len() / 4096) as u64);
    let s = r.stats();
    // ...except blocks still dirty from the write pass (at most the budget).
    let budget_blocks = (r.ram_budget() / r.block_size()) as u64;
    assert!(s.writebacks - after_write <= budget_blocks, "{s:?}");
}

#[test]
fn multithreaded_random_access() {
    let r = Arc::new(Region::new(cfg(64 * MB, 8 * MB)).unwrap());
    let words = r.len() / 8;
    let threads = 8;
    let per = words / threads;
    let hs: Vec<_> = (0..threads)
        .map(|t| {
            let r = r.clone();
            std::thread::spawn(move || {
                let p = r.as_ptr() as *mut u64;
                let base = t * per;
                let mut x = 0x1234_5678u64 + t as u64;
                for _ in 0..20_000 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let i = base + (x as usize % per);
                    unsafe { *p.add(i) = pattern(i) };
                }
                // Verify every written word (unwritten words are zero).
                for i in base..base + per {
                    let v = unsafe { *p.add(i) };
                    assert!(v == 0 || v == pattern(i), "thread {t} word {i}: {v:#x}");
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert!(r.stats().evictions > 0);
}

#[test]
fn shared_reads_across_threads() {
    let r = Arc::new(Region::new(cfg(48 * MB, 8 * MB)).unwrap());
    let words = r.len() / 8;
    unsafe {
        let p = r.as_ptr() as *mut u64;
        for i in 0..words {
            *p.add(i) = pattern(i);
        }
    }
    let hs: Vec<_> = (0..6)
        .map(|t| {
            let r = r.clone();
            std::thread::spawn(move || {
                let p = r.as_ptr() as *const u64;
                let mut x = 99u64 + t;
                for _ in 0..30_000 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let i = x as usize % words;
                    assert_eq!(unsafe { *p.add(i) }, pattern(i));
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
}

#[test]
fn explicit_api_roundtrip() {
    let r = Region::new(cfg(32 * MB, 4 * bs().max(MB))).unwrap();
    let data: Vec<u8> = (0..r.len()).map(|i| (i * 31 % 251) as u8).collect();
    r.write_at(0, &data);
    let mut back = vec![0u8; r.len()];
    r.read_at(0, &mut back);
    assert!(back == data);
    // Unaligned spans across block boundaries.
    let off = r.block_size() - 3;
    r.write_at(off, b"hello world");
    let mut b = [0u8; 11];
    r.read_at(off, &mut b);
    assert_eq!(&b, b"hello world");
    // Transparent view sees explicit writes.
    let s = unsafe { std::slice::from_raw_parts(r.as_ptr().add(off), 11) };
    assert_eq!(s, b"hello world");
    // Pins expose arena offsets for GPU uploads.
    let pin = r.pin(3);
    let (arena, alen) = r.arena();
    assert!(pin.arena_offset() + r.block_size() <= alen);
    assert_eq!(
        unsafe { arena.add(pin.arena_offset()) } as *const u8,
        pin.as_ptr()
    );
}

#[test]
fn cyclic_sweep_beats_lru() {
    // Region 4x the budget, swept repeatedly. LRU/CLOCK would reload every
    // block every pass; the cyclic policy keeps a stable subset resident.
    let mut c = cfg(64 * MB, 16 * MB);
    c.policy = Policy::Cyclic;
    c.prefetch_depth = 4;
    let r = Region::new(c).unwrap();
    let p = r.as_ptr();
    unsafe {
        for off in (0..r.len()).step_by(4096) {
            *p.add(off) = (off / 4096) as u8;
        }
    }
    r.flush().unwrap();
    let before = r.stats().bytes_read;
    let passes = 4;
    for _ in 0..passes {
        let mut acc = 0u64;
        unsafe {
            for off in (0..r.len()).step_by(4096) {
                acc = acc.wrapping_add(*p.add(off) as u64);
            }
        }
        std::hint::black_box(acc);
    }
    let read = r.stats().bytes_read - before;
    let lru = (passes * r.len()) as u64;
    assert!(read < lru * 9 / 10, "cyclic read {read} vs LRU {lru}");
}

#[test]
fn persist_and_reopen() {
    let path = tmp().join("persist-test.bin");
    let _ = std::fs::remove_file(&path);
    {
        let mut c = cfg(16 * MB, 4 * bs().max(MB));
        c.backing = path.clone();
        c.persist = true;
        let r = Region::new(c).unwrap();
        r.write_at(12345, b"persisted bytes");
        r.write_at(r.len() - 4, b"tail");
    }
    {
        let mut c = cfg(16 * MB, 4 * bs().max(MB));
        c.backing = path.clone();
        c.persist = true;
        let r = Region::new(c).unwrap();
        let s = unsafe { std::slice::from_raw_parts(r.as_ptr().add(12345), 15) };
        assert_eq!(s, b"persisted bytes");
        let mut t = [0u8; 4];
        r.read_at(r.len() - 4, &mut t);
        assert_eq!(&t, b"tail");
    }
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn read_only_file_mapping() {
    let path = tmp().join("ro-test.bin");
    let n = 20 * MB + 12345; // not block aligned
    let data: Vec<u8> = (0..n).map(|i| (i % 253) as u8).collect();
    std::fs::write(&path, &data).unwrap();
    let mut c = Config::read_only_file(&path, 4 * bs().max(MB));
    c.block_size = bs();
    let r = Region::new(c).unwrap();
    assert!(r.len() >= n);
    let s = unsafe { std::slice::from_raw_parts(r.as_ptr(), n) };
    assert!(s == &data[..]);
    assert_eq!(r.stats().writebacks, 0);
    drop(r);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn advice_cold_and_hot() {
    let r = Region::new(cfg(16 * MB, 8 * bs().max(MB))).unwrap();
    r.write_at(0, &[7u8; 4096]);
    r.advise(0, r.block_size(), Advice::Cold);
    assert!(r.stats().writebacks >= 1);
    let mut b = [0u8; 4096];
    r.read_at(0, &mut b);
    assert_eq!(b, [7u8; 4096]);
    r.advise(0, r.block_size(), Advice::Hot);
    r.advise(0, r.len(), Advice::WillNeed);
}

#[test]
fn write_governor_throttles() {
    let mut c = cfg(16 * MB, 4 * bs());
    c.write_rate_limit = 40 * MB as u64; // 40 MB/s
    let r = Region::new(c).unwrap();
    let t = std::time::Instant::now();
    let data = vec![1u8; r.len()];
    r.write_at(0, &data);
    r.flush().unwrap();
    // ~16 MB at 40 MB/s, minus the burst allowance.
    assert!(t.elapsed().as_millis() > 200, "{:?}", t.elapsed());
    assert!(r.stats().throttled_ns > 0);
}

#[test]
fn auto_policy_leaves_cyclic_after_random_burst() {
    // A one-off sequential fill bigger than RAM turns the cyclic policy on;
    // the random phase that follows must turn it off again so the whole
    // budget is available to CLOCK.
    let r = Region::new(cfg(64 * MB, 8 * MB)).unwrap();
    let p = r.as_ptr();
    unsafe {
        for off in (0..r.len()).step_by(4096) {
            *p.add(off) = 1;
        }
    }
    assert!(r.stats().cyclic, "sequential fill should enable cyclic");
    let blocks = r.len() / r.block_size();
    let mut x = 12345u64;
    for _ in 0..4 * blocks {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let b = x as usize % blocks;
        unsafe { std::ptr::read_volatile(p.add(b * r.block_size())) };
    }
    assert!(!r.stats().cyclic, "random burst should disable cyclic");
}

#[test]
fn tiny_budget_with_coalesced_readahead() {
    // 4 frames and 4 I/O threads: coalesced runs must not deadlock.
    let mut c = cfg(16 * MB, 4 * bs());
    c.io_threads = 4;
    c.prefetch_depth = 8;
    let r = Region::new(c).unwrap();
    let data: Vec<u8> = (0..r.len()).map(|i| (i % 249) as u8).collect();
    r.write_at(0, &data);
    r.flush().unwrap();
    for _ in 0..2 {
        let s = unsafe { std::slice::from_raw_parts(r.as_ptr(), r.len()) };
        assert!(s == &data[..]);
    }
}

#[test]
fn health_drives_governor_and_readahead() {
    use ddram::smart::{critical, HealthLog};
    use ddram::{HealthPolicy, HealthStatus};
    let r = Region::new(cfg(16 * MB, 8 * bs().max(MB))).unwrap();
    let healthy = HealthLog {
        composite_temp_k: 310,
        available_spare_pct: 100,
        available_spare_threshold_pct: 10,
        percentage_used: 10,
        data_units_written: 200_000_000, // ~102 TB written at 10% used
        power_on_hours: 8760,
        ..Default::default()
    };
    let p = HealthPolicy::default();
    match r.apply_health(&healthy, &p) {
        HealthStatus::Ok { write_rate } => {
            // ~1024 TB total, ~922 TB left over 4 years, 25% share: ~1.6 MB/s.
            assert!((1_000_000..3_000_000).contains(&write_rate), "{write_rate}");
        }
        s => panic!("{s:?}"),
    }
    let hot = HealthLog {
        critical_warning: critical::TEMPERATURE,
        ..healthy.clone()
    };
    assert!(matches!(r.apply_health(&hot, &p), HealthStatus::Hot { .. }));
    let failing = HealthLog {
        critical_warning: critical::READ_ONLY,
        ..healthy
    };
    match r.apply_health(&failing, &p) {
        HealthStatus::Unsafe { warnings } => assert!(!warnings.is_empty()),
        s => panic!("{s:?}"),
    }
}
