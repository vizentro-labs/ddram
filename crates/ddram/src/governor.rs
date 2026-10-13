// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Endurance-aware write governor.
//!
//! NAND wears out with program/erase cycles, so a RAM extension that writes
//! back carelessly can consume a consumer SSD's rated endurance (TBW) in
//! months. The governor is a token bucket over bytes written back to the
//! backing device. The rate can be set directly or derived from the drive's
//! NVMe health log (see [`crate::smart::EnduranceBudget`]).
//!
//! `charge` may be called from the fault handler, so it never allocates and
//! only sleeps with async-signal-safe primitives.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::sys;

pub struct WriteGovernor {
    /// Bytes per second; 0 = unlimited.
    rate: AtomicU64,
    /// Maximum burst in bytes.
    burst: AtomicU64,
    /// Virtual time (ns since `epoch`) at which the bucket is empty.
    debt_ns: AtomicU64,
    epoch: Instant,
    pub throttled_ns: AtomicU64,
}

impl WriteGovernor {
    pub fn new(rate_bytes_per_sec: u64, burst_bytes: u64) -> Self {
        WriteGovernor {
            rate: AtomicU64::new(rate_bytes_per_sec),
            burst: AtomicU64::new(burst_bytes),
            debt_ns: AtomicU64::new(0),
            epoch: Instant::now(),
            throttled_ns: AtomicU64::new(0),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(0, 0)
    }

    pub fn set_rate(&self, rate_bytes_per_sec: u64, burst_bytes: u64) {
        self.rate.store(rate_bytes_per_sec, Ordering::Relaxed);
        self.burst.store(burst_bytes, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// True when the next write would have to wait.
    pub fn is_constrained(&self) -> bool {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return false;
        }
        let now = self.epoch.elapsed().as_nanos() as u64;
        self.debt_ns.load(Ordering::Relaxed) > now
    }

    /// Account for `bytes` about to be written, sleeping if over budget.
    pub fn charge(&self, bytes: u64) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        let burst_ns = self
            .burst
            .load(Ordering::Relaxed)
            .saturating_mul(1_000_000_000)
            / rate;
        let cost_ns = bytes.saturating_mul(1_000_000_000) / rate;
        let now = self.epoch.elapsed().as_nanos() as u64;
        // GCRA: debt never lags `now - burst`, so idle time accrues at most `burst`.
        let mut cur = self.debt_ns.load(Ordering::Relaxed);
        let target = loop {
            let base = cur.max(now.saturating_sub(burst_ns));
            let next = base + cost_ns;
            match self
                .debt_ns
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => break next,
                Err(v) => cur = v,
            }
        };
        let allowed_at = target.saturating_sub(burst_ns);
        if allowed_at > now {
            let wait = allowed_at - now;
            self.throttled_ns.fetch_add(wait, Ordering::Relaxed);
            sys::sleep_us(wait / 1000);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let g = WriteGovernor::unlimited();
        let t = Instant::now();
        for _ in 0..1000 {
            g.charge(1 << 30);
        }
        assert!(t.elapsed().as_millis() < 50);
    }

    #[test]
    fn rate_limits_after_burst() {
        // 100 MB/s with a 1 MB burst: writing 6 MB takes ~50 ms.
        let g = WriteGovernor::new(100_000_000, 1_000_000);
        let t = Instant::now();
        for _ in 0..6 {
            g.charge(1_000_000);
        }
        let ms = t.elapsed().as_millis();
        assert!((40..200).contains(&ms), "took {ms} ms");
        assert!(g.throttled_ns.load(Ordering::Relaxed) > 0);
    }
}
