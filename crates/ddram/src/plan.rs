// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Access plans: the application tells the pager the order in which it will
//! touch the region, and the pager pages optimally instead of guessing.
//!
//! A [`Plan`] is an ordered list of byte ranges, optionally cyclic (the whole
//! sequence repeats, as every decode step of a language model reads every
//! layer again). Installed with `Region::set_plan`, it drives two things:
//!
//! * **Prefetch**: the next `lookahead` bytes of the plan are kept resident,
//!   read in plan order with coalesced I/O, so the application never waits
//!   for the SSD while the plan holds. The heuristic stream detector is off
//!   while a plan is active, so nothing outside the plan is read.
//! * **Eviction**: Belady's optimal rule. The block whose next planned use is
//!   furthest in the future is evicted first; blocks the plan never touches
//!   again go before any planned block. For a cyclic sweep larger than RAM
//!   this keeps a stable prefix resident and streams the rest, which is what
//!   the cyclic policy approximates heuristically; for non-sequential plans
//!   (mixture-of-experts routing, asset streaming along a path) no heuristic
//!   policy comes close.
//!
//! Ranges should be the units the application consumes together (a layer, an
//! expert, an asset). Several threads may consume one range concurrently. The
//! plan position is advanced by page faults, in O(1), so it costs nothing on
//! resident data.

use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};

/// "Not planned" marker for `next_use` / `next_occ`.
pub(crate) const NONE: u32 = u32::MAX;

/// An ordered list of byte ranges the application is going to access.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    ranges: Vec<(u64, u64)>,
    cyclic: bool,
    lookahead: u64,
}

const MAGIC: &[u8; 8] = b"DDRMPLAN";
const VERSION: u32 = 1;

impl Plan {
    /// An empty, non-cyclic plan.
    pub fn new() -> Plan {
        Plan::default()
    }

    /// An empty plan that repeats from the start once consumed (LLM decode
    /// loops, per-frame asset passes).
    pub fn cyclic() -> Plan {
        Plan {
            cyclic: true,
            ..Plan::default()
        }
    }

    pub fn is_cyclic(&self) -> bool {
        self.cyclic
    }

    pub fn set_cyclic(&mut self, cyclic: bool) -> &mut Plan {
        self.cyclic = cyclic;
        self
    }

    /// Append the byte range `[off, off+len)` as the next thing accessed.
    /// Adjacent appends of adjacent ranges are merged.
    pub fn push(&mut self, off: u64, len: u64) -> &mut Plan {
        if len == 0 {
            return self;
        }
        if let Some(last) = self.ranges.last_mut() {
            if last.0 + last.1 == off {
                last.1 += len;
                return self;
            }
        }
        self.ranges.push((off, len));
        self
    }

    /// Builder form of [`Plan::push`].
    pub fn then(mut self, off: u64, len: u64) -> Plan {
        self.push(off, len);
        self
    }

    /// How far ahead of the current position the pager keeps data resident,
    /// in bytes. 0 (the default) means a quarter of the RAM budget. Values
    /// above half the budget are clamped to it.
    pub fn set_lookahead(&mut self, bytes: u64) -> &mut Plan {
        self.lookahead = bytes;
        self
    }

    pub fn lookahead(&self) -> u64 {
        self.lookahead
    }

    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    /// Number of ranges.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Sum of all range lengths.
    pub fn total_bytes(&self) -> u64 {
        self.ranges.iter().map(|r| r.1).sum()
    }

    /// Serialize to a small binary file (for persisted access profiles).
    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let mut f = std::fs::File::create(path)?;
        let mut buf = Vec::with_capacity(32 + 16 * self.ranges.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(self.cyclic as u32).to_le_bytes());
        buf.extend_from_slice(&self.lookahead.to_le_bytes());
        buf.extend_from_slice(&(self.ranges.len() as u64).to_le_bytes());
        for &(o, l) in &self.ranges {
            buf.extend_from_slice(&o.to_le_bytes());
            buf.extend_from_slice(&l.to_le_bytes());
        }
        f.write_all(&buf)?;
        f.sync_all()
    }

    /// Read a plan written by [`Plan::save`].
    pub fn load(path: impl AsRef<Path>) -> io::Result<Plan> {
        let mut f = std::fs::File::open(path)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "not a ddram plan file");
        if buf.len() < 32 || &buf[..8] != MAGIC {
            return Err(bad());
        }
        let u32_at = |i: usize| u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
        if u32_at(8) != VERSION {
            return Err(bad());
        }
        let cyclic = u32_at(12) != 0;
        let lookahead = u64_at(16);
        let n = u64_at(24) as usize;
        if buf.len() != 32 + 16 * n {
            return Err(bad());
        }
        let mut ranges = Vec::with_capacity(n);
        for i in 0..n {
            let o = u64_at(32 + 16 * i);
            let l = u64_at(40 + 16 * i);
            if l == 0 || o.checked_add(l).is_none() {
                return Err(bad());
            }
            ranges.push((o, l));
        }
        Ok(Plan {
            ranges,
            cyclic,
            lookahead,
        })
    }

    /// Build a plan from a recorded access order (block indices, see
    /// `Region::record`). The trace is cut after the last block seen for the
    /// first time, so a recording that ran into a second pass yields one
    /// cycle; consecutive blocks merge into ranges.
    pub fn from_trace(blocks: &[u64], block_size: u64, cyclic: bool) -> Plan {
        let max = blocks.iter().copied().max().map_or(0, |m| m as usize + 1);
        let mut seen = vec![false; max];
        let mut end = 0;
        for (i, &b) in blocks.iter().enumerate() {
            if !seen[b as usize] {
                seen[b as usize] = true;
                end = i + 1;
            }
        }
        Plan::from_blocks(blocks[..end].iter().copied(), block_size, cyclic)
    }

    /// Build a plan from a sequence of block indices in access order:
    /// consecutive blocks merge into ranges.
    pub fn from_blocks(
        blocks: impl IntoIterator<Item = u64>,
        block_size: u64,
        cyclic: bool,
    ) -> Plan {
        let mut p = Plan {
            cyclic,
            ..Plan::default()
        };
        for b in blocks {
            p.push(b * block_size, block_size);
        }
        p
    }
}

/// Live state of an installed plan, in block units. Built once per
/// `set_plan`; read from the fault path through `Inner::with_plan`.
pub(crate) struct PlanState {
    /// Block index per step.
    pub steps: Box<[u32]>,
    /// Index of the range each step belongs to.
    pub range_of_step: Box<[u32]>,
    /// First step of each range, plus a sentinel equal to `steps.len()`.
    pub range_start: Box<[u32]>,
    /// Next step with the same block as step `s` (wrapping if cyclic), or `NONE`.
    pub next_occ: Box<[u32]>,
    /// Per block: the step of its next planned use, or `NONE`.
    pub next_use: Box<[AtomicU32]>,
    pub cyclic: bool,
    /// Blocks kept resident beyond the start of the current range.
    pub lookahead: usize,
    /// Absolute index of the range being consumed (monotone; `% nranges`
    /// gives the range).
    pub cursor: AtomicUsize,
    /// Ranges whose `next_use` entries have been advanced to their next
    /// occurrence (everything before `cursor`).
    pub applied: AtomicUsize,
    /// Prefetch frontier, in absolute steps (`% nsteps` gives the step).
    pub issued: AtomicUsize,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
}

impl PlanState {
    /// Expand `plan` to blocks. Ranges longer than `chunk` blocks are split
    /// so the plan position stays fine-grained inside large ranges.
    pub fn build(plan: &Plan, bs: usize, nblocks: usize, nframes: usize) -> io::Result<PlanState> {
        if plan.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty plan"));
        }
        // Look-ahead: enough to keep the SSD queue full and absorb the
        // background step's latency, but a small share of the budget so the
        // stable part of a cyclic plan stays large.
        let mut lookahead = if plan.lookahead == 0 {
            (nframes / 8).min((64 << 20) / bs).max(8)
        } else {
            (plan.lookahead as usize).div_ceil(bs)
        };
        lookahead = lookahead.clamp(1, (nframes / 2).max(1));
        // Ranges larger than a quarter of the budget are split into pieces
        // of an eighth so the plan position stays fine-grained (a single
        // huge cyclic range then behaves like the cyclic policy). Smaller
        // ranges are kept whole: a range is the unit its consumers finish
        // together, and the current one is never evicted.
        let chunk = (nframes / 8).max(1);
        let split_above = (nframes / 4).max(1);
        let mut steps: Vec<u32> = Vec::new();
        let mut range_of_step: Vec<u32> = Vec::new();
        let mut range_start: Vec<u32> = Vec::new();
        for &(off, len) in &plan.ranges {
            let first = (off / bs as u64) as usize;
            let end = (off + len).div_ceil(bs as u64) as usize;
            if first >= nblocks {
                continue;
            }
            let end = end.min(nblocks);
            let mut b = first;
            while b < end {
                let n = if end - first > split_above {
                    (end - b).min(chunk)
                } else {
                    end - b
                };
                range_start.push(steps.len() as u32);
                let r = range_start.len() as u32 - 1;
                for i in 0..n {
                    steps.push((b + i) as u32);
                    range_of_step.push(r);
                }
                b += n;
            }
        }
        if steps.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plan lies entirely outside the region",
            ));
        }
        if steps.len() >= NONE as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "plan too long"));
        }
        range_start.push(steps.len() as u32);
        let nsteps = steps.len();
        // next_occ by a backward pass; `last` then holds each block's first
        // occurrence, which is the initial next_use and the wrap target.
        let mut last = vec![NONE; nblocks];
        let mut next_occ = vec![NONE; nsteps];
        for s in (0..nsteps).rev() {
            let b = steps[s] as usize;
            next_occ[s] = last[b];
            last[b] = s as u32;
        }
        if plan.cyclic {
            for s in 0..nsteps {
                if next_occ[s] == NONE {
                    next_occ[s] = last[steps[s] as usize];
                }
            }
        }
        let next_use = last.into_iter().map(AtomicU32::new).collect();
        Ok(PlanState {
            steps: steps.into_boxed_slice(),
            range_of_step: range_of_step.into_boxed_slice(),
            range_start: range_start.into_boxed_slice(),
            next_occ: next_occ.into_boxed_slice(),
            next_use,
            cyclic: plan.cyclic,
            lookahead,
            cursor: AtomicUsize::new(0),
            applied: AtomicUsize::new(0),
            issued: AtomicUsize::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    #[inline]
    pub fn nsteps(&self) -> usize {
        self.steps.len()
    }

    #[inline]
    pub fn nranges(&self) -> usize {
        self.range_start.len() - 1
    }

    /// First step of the current range (within the cycle).
    #[inline]
    pub fn pos(&self) -> usize {
        self.range_start[self.cursor.load(Relaxed) % self.nranges()] as usize
    }

    /// Steps from the current position to step `s` going forward, or `NONE`
    /// (as usize) if `s` is behind in a non-cyclic plan.
    #[inline]
    pub fn forward(&self, pos: usize, s: usize) -> usize {
        if s >= pos {
            s - pos
        } else if self.cyclic {
            s + self.nsteps() - pos
        } else {
            NONE as usize
        }
    }

    /// Eviction key of a block: larger means a better victim. Blocks in the
    /// current range score 0 (in use); blocks the plan does not touch again
    /// score above every planned block, unreferenced ones highest.
    #[inline]
    pub fn victim_key(&self, block: usize, referenced: bool) -> usize {
        let s = self.next_use[block].load(Relaxed);
        let n = self.nsteps();
        let far = n + 1 + (!referenced) as usize;
        if s == NONE {
            return far;
        }
        let s = s as usize;
        let cur = self.cursor.load(Relaxed) % self.nranges();
        if self.range_of_step[s] as usize == cur {
            return 0;
        }
        let d = self.forward(self.range_start[cur] as usize, s);
        if d == NONE as usize {
            far
        } else {
            d
        }
    }

    /// Called from the fault path on an access to `block`. Moves the cursor
    /// forward when the block's next planned use lies ahead of the current
    /// range (but less than half a cycle away, so threads lagging behind the
    /// frontier do not drag it back). Returns whether the block is planned.
    #[inline]
    pub fn access(&self, block: usize) -> bool {
        let s = self.next_use[block].load(Relaxed);
        if s == NONE {
            self.misses.fetch_add(1, Relaxed);
            return false;
        }
        self.hits.fetch_add(1, Relaxed);
        let s = s as usize;
        let n = self.nsteps();
        let nr = self.nranges();
        let cur_abs = self.cursor.load(Relaxed);
        let cur = cur_abs % nr;
        let pos = self.range_start[cur] as usize;
        let end = self.range_start[cur + 1] as usize;
        let d = self.forward(pos, s);
        if d == NONE as usize || d < end - pos || (self.cyclic && d > n / 2) {
            return true;
        }
        let rs = self.range_of_step[s] as usize;
        let jump = if rs >= cur { rs - cur } else { rs + nr - cur };
        self.cursor.fetch_max(cur_abs + jump, Relaxed);
        true
    }

    /// Whether `block` is planned within twice the look-ahead of the current
    /// position (used to bound coalesced reads to what the plan needs).
    #[inline]
    pub fn soon(&self, block: usize) -> bool {
        let s = self.next_use[block].load(Relaxed);
        if s == NONE {
            return false;
        }
        let d = self.forward(self.pos(), s as usize);
        d != NONE as usize && d <= 2 * self.lookahead
    }

    /// Absolute step of the start of absolute range `r`.
    #[inline]
    pub fn abs_start(&self, r: usize) -> usize {
        let nr = self.nranges();
        (r / nr) * self.nsteps() + self.range_start[r % nr] as usize
    }
}

/// Block-index recorder for `Region::record`: a preallocated buffer the
/// fault path appends to without allocating.
pub(crate) struct Recorder {
    buf: Box<[AtomicU32]>,
    len: AtomicUsize,
}

impl Recorder {
    pub fn new(cap: usize) -> Recorder {
        Recorder {
            buf: (0..cap.max(1)).map(|_| AtomicU32::new(NONE)).collect(),
            len: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub fn push(&self, block: usize) {
        let i = self.len.fetch_add(1, Relaxed);
        if i < self.buf.len() {
            self.buf[i].store(block as u32, Relaxed);
        }
    }

    /// Entries recorded so far, in order (slots still being written are
    /// skipped).
    pub fn blocks(&self) -> Vec<u64> {
        let n = self.len.load(Relaxed).min(self.buf.len());
        self.buf[..n]
            .iter()
            .map(|b| b.load(Relaxed))
            .filter(|&b| b != NONE)
            .map(u64::from)
            .collect()
    }

    pub fn is_full(&self) -> bool {
        self.len.load(Relaxed) >= self.buf.len()
    }
}
