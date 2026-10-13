// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! The paging engine shared by the transparent (fault-driven) and explicit
//! (pin/read/write) interfaces.
//!
//! # Model
//!
//! * The region is split into fixed-size **blocks** (default 1 MiB). Each block
//!   has one `AtomicU64` state word: a tag, a dirty bit, a mapped bit and the
//!   index of the frame that holds it.
//! * RAM is a fixed **arena** of `nframes` frames of block size. Only the arena
//!   consumes physical memory, so the RAM budget is a hard bound.
//! * A resident block is either *mapped* (its frame is mapped over the block's
//!   address in the application view) or *primed* (loaded by prefetch or by the
//!   explicit API but not yet mapped). The first touch of a primed block is a
//!   cheap "soft" fault that maps it; this gives the engine precise access
//!   information without hardware accessed bits.
//! * Clean blocks are mapped read-only; the first write faults and marks the
//!   block dirty. Only dirty blocks are ever written back, which keeps NAND
//!   wear proportional to what the application actually modified.
//!
//! # Concurrency
//!
//! All transitions are CAS on the state word. Frame allocation (clock hand)
//! is serialized by a spinlock that is never held across I/O. The fault path
//! never allocates, so it can run inside a signal handler.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};

use crossbeam_queue::ArrayQueue;

use crate::backing::Backing;
use crate::governor::WriteGovernor;
use crate::sys;

// State word layout.
const TAG_MASK: u64 = 0xff;
pub(crate) const EMPTY: u64 = 0;
pub(crate) const LOADING: u64 = 1;
pub(crate) const RESIDENT: u64 = 2;
pub(crate) const EVICTING: u64 = 3;
pub(crate) const BUSY: u64 = 4;
const DIRTY: u64 = 1 << 8;
const MAPPED: u64 = 1 << 9;

#[inline]
fn tag(s: u64) -> u64 {
    s & TAG_MASK
}
#[inline]
fn frame_of(s: u64) -> usize {
    (s >> 32) as usize
}
#[inline]
fn is_dirty(s: u64) -> bool {
    s & DIRTY != 0
}
#[inline]
fn is_mapped(s: u64) -> bool {
    s & MAPPED != 0
}
#[inline]
fn mk(tag: u64, frame: usize, dirty: bool, mapped: bool) -> u64 {
    tag | ((frame as u64) << 32) | if dirty { DIRTY } else { 0 } | if mapped { MAPPED } else { 0 }
}

const FREE: u64 = u64::MAX;
const CLAIMED: u64 = u64::MAX - 1;

struct Frame {
    block: AtomicU64,
    pins: AtomicU32,
    referenced: AtomicBool,
    sticky: AtomicBool,
    /// Pinned resident by `Advice::Hot` (independent of the cyclic policy).
    hot: AtomicBool,
    /// Loaded by sequential readahead (eligible for write-back clustering).
    seq: AtomicBool,
}

#[derive(Default)]
pub(crate) struct Counters {
    pub hard_faults: AtomicU64,
    pub soft_faults: AtomicU64,
    pub write_faults: AtomicU64,
    pub loads: AtomicU64,
    pub prefetch_loads: AtomicU64,
    pub zero_fills: AtomicU64,
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    pub evictions: AtomicU64,
    pub writebacks: AtomicU64,
    pub wait_spins: AtomicU64,
    pub direct_reclaims: AtomicU64,
}

/// Replacement policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// CLOCK (second chance), switching a region to `Cyclic` when a
    /// sequential scan longer than the RAM budget is detected.
    Auto,
    /// CLOCK only.
    Clock,
    /// Scan-resistant: keep a stable subset resident and stream the rest
    /// through a small window. Optimal-ish for repeated sweeps larger than RAM
    /// (LLM layer weights each token, full-scene asset passes), where LRU and
    /// CLOCK degrade to a 0% hit rate.
    Cyclic,
}

const STREAMS: usize = 8;

/// Largest coalesced readahead I/O.
pub(crate) const MAX_IO_BYTES: usize = 2 << 20;
const MAX_RUN: usize = 32;

pub(crate) struct Inner {
    pub base: usize,
    pub len: usize,
    pub bs: usize,
    shift: u32,
    pub nblocks: usize,
    pub nframes: usize,
    pub arena: sys::Arena,
    states: Box<[AtomicU64]>,
    frames: Box<[Frame]>,
    on_disk: Box<[AtomicU64]>,
    alloc_lock: AtomicBool,
    /// Frames ready for immediate use, kept stocked by the reclaimer.
    free: ArrayQueue<usize>,
    free_low: usize,
    free_high: usize,
    hand: AtomicUsize,
    pub backing: Backing,
    pub governor: WriteGovernor,
    pub stats: Counters,
    pub queue: ArrayQueue<u64>,
    pub depth: AtomicUsize,
    /// Longest coalesced readahead run (blocks). Bounded so that I/O workers
    /// holding partially-allocated runs can never claim every frame.
    max_run: usize,
    stream_last: [AtomicU64; STREAMS],
    stream_run: [AtomicU64; STREAMS],
    stream_next: AtomicUsize,
    /// Forward-stream accesses (all streams) since the last random burst.
    seq_accesses: AtomicUsize,
    /// Score of recent accesses that continued no stream (see `auto_policy`).
    random_streak: AtomicUsize,
    pub policy: Policy,
    cyclic: AtomicBool,
    sticky_count: AtomicUsize,
    sticky_quota: usize,
    pub read_only: bool,
    pub shutdown: AtomicBool,
}

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

pub(crate) struct InnerParams {
    pub len: usize,
    pub bs: usize,
    pub nframes: usize,
    pub backing: Backing,
    pub governor: WriteGovernor,
    pub depth: usize,
    pub io_threads: usize,
    pub policy: Policy,
    pub existing_data: bool,
}

fn fatal(msg: &str, e: Option<std::io::Error>) -> ! {
    sys::raw_stderr(b"ddram: fatal: ");
    sys::raw_stderr(msg.as_bytes());
    if let Some(e) = e {
        // Formatting allocates, but we are about to abort anyway.
        sys::raw_stderr(format!(": {e}").as_bytes());
    }
    sys::raw_stderr(b"\n");
    std::process::abort();
}

impl Inner {
    pub fn new(p: InnerParams) -> std::io::Result<Inner> {
        assert!(p.bs.is_power_of_two());
        let nblocks = p.len / p.bs;
        let arena = sys::create_arena(p.nframes * p.bs)?;
        let base = match sys::reserve(p.len, p.bs, p.bs) {
            Ok(b) => b as usize,
            Err(e) => {
                sys::destroy_arena(&arena);
                return Err(e);
            }
        };
        let states = (0..nblocks).map(|_| AtomicU64::new(EMPTY)).collect();
        let frames = (0..p.nframes)
            .map(|_| Frame {
                block: AtomicU64::new(FREE),
                pins: AtomicU32::new(0),
                referenced: AtomicBool::new(false),
                sticky: AtomicBool::new(false),
                hot: AtomicBool::new(false),
                seq: AtomicBool::new(false),
            })
            .collect();
        let fill = if p.existing_data { u64::MAX } else { 0 };
        let on_disk = (0..nblocks.div_ceil(64))
            .map(|_| AtomicU64::new(fill))
            .collect();
        // Frames left for streaming once the cyclic policy pins a stable
        // subset: room for every tracked stream's readahead plus the requests
        // in flight, so prefetched blocks are not evicted before use.
        let window = (p.depth.max(1) * (STREAMS + p.io_threads) + 4)
            .max(p.nframes / 32)
            .min(p.nframes / 2);
        let free = ArrayQueue::new(p.nframes);
        for f in 0..p.nframes {
            let _ = free.push(f);
        }
        let free_low = (p.nframes / 64).clamp(2, 4096);
        let free_high = (2 * free_low).min(p.nframes / 4).max(free_low + 1);
        Ok(Inner {
            base,
            len: p.len,
            bs: p.bs,
            shift: p.bs.trailing_zeros(),
            nblocks,
            nframes: p.nframes,
            arena,
            states,
            frames,
            on_disk,
            alloc_lock: AtomicBool::new(false),
            free,
            free_low,
            free_high,
            hand: AtomicUsize::new(0),
            read_only: p.backing.read_only,
            backing: p.backing,
            governor: p.governor,
            stats: Counters::default(),
            queue: ArrayQueue::new(1024),
            depth: AtomicUsize::new(p.depth),
            max_run: (p.nframes / (2 * (p.io_threads + 1)))
                .min(MAX_IO_BYTES / p.bs)
                .clamp(1, MAX_RUN),
            stream_last: std::array::from_fn(|_| AtomicU64::new(u64::MAX - 1)),
            stream_run: std::array::from_fn(|_| AtomicU64::new(0)),
            stream_next: AtomicUsize::new(0),
            seq_accesses: AtomicUsize::new(0),
            random_streak: AtomicUsize::new(0),
            policy: p.policy,
            cyclic: AtomicBool::new(p.policy == Policy::Cyclic),
            sticky_count: AtomicUsize::new(0),
            sticky_quota: p.nframes.saturating_sub(window),
            shutdown: AtomicBool::new(false),
        })
    }

    #[inline]
    fn block_addr(&self, b: usize) -> *mut u8 {
        (self.base + (b << self.shift)) as *mut u8
    }
    #[inline]
    pub fn frame_ptr(&self, f: usize) -> *mut u8 {
        unsafe { self.arena.ptr.add(f << self.shift) }
    }
    #[inline]
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.len
    }
    pub fn is_cyclic(&self) -> bool {
        self.cyclic.load(Relaxed)
    }

    fn on_disk(&self, b: usize) -> bool {
        self.on_disk[b / 64].load(Relaxed) & (1 << (b % 64)) != 0
    }
    fn set_on_disk(&self, b: usize) {
        self.on_disk[b / 64].fetch_or(1 << (b % 64), Relaxed);
    }

    fn lock(&self) {
        while self
            .alloc_lock
            .compare_exchange_weak(false, true, Acquire, Relaxed)
            .is_err()
        {
            while self.alloc_lock.load(Relaxed) {
                std::hint::spin_loop();
            }
        }
    }
    fn unlock(&self) {
        self.alloc_lock.store(false, Release);
    }

    fn backoff(&self, spins: &mut u32) {
        *spins += 1;
        self.stats.wait_spins.fetch_add(1, Relaxed);
        if *spins < 64 {
            std::hint::spin_loop();
        } else if *spins < 256 {
            sys::yield_now();
        } else {
            sys::sleep_us(20);
        }
    }

    // ------------------------------------------------------------------
    // Fault path (may run in a signal / exception handler)
    // ------------------------------------------------------------------

    pub fn handle_fault(&self, addr: usize, write: Option<bool>) -> bool {
        let b = (addr - self.base) >> self.shift;
        if b >= self.nblocks {
            return false;
        }
        let st = &self.states[b];
        let mut spins = 0;
        let mut noted = false;
        loop {
            let s = st.load(Acquire);
            match tag(s) {
                EMPTY => {
                    if st
                        .compare_exchange(s, mk(LOADING, 0, false, false), AcqRel, Relaxed)
                        .is_ok()
                    {
                        self.stats.hard_faults.fetch_add(1, Relaxed);
                        self.note_access(b);
                        self.load(b, true, write == Some(true), false);
                        return true;
                    }
                }
                RESIDENT => {
                    let f = frame_of(s);
                    if !is_mapped(s) {
                        if st
                            .compare_exchange(s, mk(BUSY, f, is_dirty(s), false), AcqRel, Relaxed)
                            .is_ok()
                        {
                            if !noted {
                                self.stats.soft_faults.fetch_add(1, Relaxed);
                                self.note_access(b);
                            }
                            let w = !self.read_only && (is_dirty(s) || write == Some(true));
                            self.frames[f].referenced.store(true, Relaxed);
                            if let Err(e) = unsafe {
                                sys::map_frame(
                                    &self.arena,
                                    f << self.shift,
                                    self.block_addr(b),
                                    self.bs,
                                    w,
                                )
                            } {
                                fatal(
                                    "map_frame failed (raise vm.max_map_count or the block size)",
                                    Some(e),
                                );
                            }
                            st.store(mk(RESIDENT, f, is_dirty(s) || w, true), Release);
                            return true;
                        }
                    } else {
                        if write == Some(false) {
                            return true; // raced with a concurrent map
                        }
                        if self.read_only {
                            return false;
                        }
                        if st
                            .compare_exchange(s, mk(BUSY, f, true, true), AcqRel, Relaxed)
                            .is_ok()
                        {
                            self.stats.write_faults.fetch_add(1, Relaxed);
                            if let Err(e) = unsafe { sys::protect_rw(self.block_addr(b), self.bs) }
                            {
                                fatal("mprotect failed", Some(e));
                            }
                            st.store(mk(RESIDENT, f, true, true), Release);
                            return true;
                        }
                    }
                }
                _ => {
                    if !noted {
                        noted = true;
                        self.note_access(b);
                    }
                    self.backoff(&mut spins);
                }
            }
        }
    }

    /// Fill a block into a frame. Caller has moved the state to `LOADING`.
    fn load(&self, b: usize, map: bool, write: bool, pin: bool) -> usize {
        let f = self.alloc_frame();
        if self.on_disk(b) {
            let buf = unsafe { std::slice::from_raw_parts_mut(self.frame_ptr(f), self.bs) };
            if let Err(e) = self.backing.read_block((b as u64) << self.shift, buf) {
                fatal("backing read failed", Some(e));
            }
            self.stats.bytes_read.fetch_add(self.bs as u64, Relaxed);
        } else {
            unsafe { ptr::write_bytes(self.frame_ptr(f), 0, self.bs) };
            self.stats.zero_fills.fetch_add(1, Relaxed);
        }
        self.publish(b, f, map, write, pin, map || pin);
        f
    }

    /// Make a filled frame visible as block `b`.
    fn publish(&self, b: usize, f: usize, map: bool, write: bool, pin: bool, referenced: bool) {
        self.stats.loads.fetch_add(1, Relaxed);
        let fr = &self.frames[f];
        // Demand loads are not part of a stream; readahead sets this after.
        fr.seq.store(false, Relaxed);
        if pin {
            fr.pins.fetch_add(1, SeqCst);
        }
        fr.referenced.store(referenced, Relaxed);
        if self.cyclic.load(Relaxed) && self.sticky_count.load(Relaxed) < self.sticky_quota {
            self.sticky_count.fetch_add(1, Relaxed);
            fr.sticky.store(true, Relaxed);
        }
        fr.block.store(b as u64, Release);
        let w = map && write && !self.read_only;
        if map {
            if let Err(e) = unsafe {
                sys::map_frame(&self.arena, f << self.shift, self.block_addr(b), self.bs, w)
            } {
                fatal(
                    "map_frame failed (raise vm.max_map_count or the block size)",
                    Some(e),
                );
            }
        }
        self.states[b].store(mk(RESIDENT, f, w, map), Release);
    }

    /// Claim a frame for a new block: from the free pool the reclaimer keeps
    /// stocked if possible, otherwise by direct reclaim on this thread.
    fn alloc_frame(&self) -> usize {
        while let Some(f) = self.free.pop() {
            if self.frames[f]
                .block
                .compare_exchange(FREE, CLAIMED, AcqRel, Relaxed)
                .is_ok()
            {
                return f;
            }
        }
        self.stats.direct_reclaims.fetch_add(1, Relaxed);
        let mut spins = 0;
        loop {
            if let Some(f) = self.reclaim_one(false) {
                return f;
            }
            self.backoff(&mut spins); // every frame pinned or in flight
        }
    }

    /// Run CLOCK to pick a victim, evict it, and return its frame `CLAIMED`.
    /// With `for_pool`, frames that are already free are skipped.
    fn reclaim_one(&self, for_pool: bool) -> Option<usize> {
        let n = self.nframes;
        let prefer_clean = self.governor.is_constrained();
        let cyclic = self.cyclic.load(Relaxed);
        self.lock();
        let mut victim = None;
        for i in 0..3 * n {
            let f = self.hand.load(Relaxed);
            self.hand.store(if f + 1 == n { 0 } else { f + 1 }, Relaxed);
            let fr = &self.frames[f];
            let ob = fr.block.load(Acquire);
            if ob == FREE {
                if !for_pool
                    && fr
                        .block
                        .compare_exchange(FREE, CLAIMED, AcqRel, Relaxed)
                        .is_ok()
                {
                    victim = Some((f, None));
                    break;
                }
                continue;
            }
            if ob == CLAIMED || fr.pins.load(SeqCst) > 0 {
                continue;
            }
            let pass = i / n;
            // Sticky frames only bind while the cyclic policy is active;
            // hot frames always do (until every other frame is pinned).
            let sticky = fr.sticky.load(Relaxed);
            if pass < 2 && ((sticky && cyclic) || fr.hot.load(Relaxed)) {
                continue;
            }
            if pass == 0 && fr.referenced.swap(false, Relaxed) {
                continue;
            }
            let st = &self.states[ob as usize];
            let s = st.load(Acquire);
            if tag(s) != RESIDENT || frame_of(s) != f {
                continue;
            }
            if pass == 0 && prefer_clean && is_dirty(s) {
                continue;
            }
            let ev = mk(EVICTING, f, is_dirty(s), is_mapped(s));
            if st.compare_exchange(s, ev, SeqCst, Relaxed).is_err() {
                continue;
            }
            if fr.pins.load(SeqCst) > 0 {
                st.store(s, Release);
                continue;
            }
            fr.block.store(CLAIMED, Relaxed);
            if sticky {
                fr.sticky.store(false, Relaxed);
                self.sticky_count.fetch_sub(1, Relaxed);
            }
            fr.hot.store(false, Relaxed);
            victim = Some((f, Some((ob as usize, s))));
            break;
        }
        self.unlock();
        let (f, old) = victim?;
        if let Some((ob, s)) = old {
            if is_dirty(s) && self.max_run > 1 {
                self.evict_cluster(ob, f, s);
            } else {
                self.evict(ob, f, s);
            }
        }
        Some(f)
    }

    /// Evict dirty block `b` together with the following dirty blocks that
    /// came from the same sequential stream and that CLOCK would pick next
    /// anyway (readahead-loaded, unreferenced, unpinned, not hot/sticky),
    /// writing the whole run back with one vectored write. Extra frames go to
    /// the free pool. Without this, a sequential write stream of small blocks
    /// is written back one small I/O at a time.
    fn evict_cluster(&self, b: usize, f: usize, s: u64) {
        let cyclic = self.cyclic.load(Relaxed);
        let mut blocks = [0usize; MAX_RUN];
        let mut frames = [0usize; MAX_RUN];
        let mut states = [0u64; MAX_RUN];
        blocks[0] = b;
        frames[0] = f;
        states[0] = s;
        let mut n = 1;
        let max = self.max_run.min(MAX_IO_BYTES >> self.shift).max(1);
        while n < max {
            let nb = b + n;
            if nb >= self.nblocks {
                break;
            }
            let st = &self.states[nb];
            let ns = st.load(Acquire);
            if tag(ns) != RESIDENT || !is_dirty(ns) {
                break;
            }
            let nf = frame_of(ns);
            let fr = &self.frames[nf];
            if !fr.seq.load(Relaxed)
                || fr.pins.load(SeqCst) > 0
                || fr.referenced.load(Relaxed)
                || fr.hot.load(Relaxed)
                || (cyclic && fr.sticky.load(Relaxed))
            {
                break;
            }
            let ev = mk(EVICTING, nf, true, is_mapped(ns));
            if st.compare_exchange(ns, ev, SeqCst, Relaxed).is_err() {
                break;
            }
            if fr.pins.load(SeqCst) > 0 {
                st.store(ns, Release);
                break;
            }
            fr.block.store(CLAIMED, Relaxed);
            if fr.sticky.swap(false, Relaxed) {
                self.sticky_count.fetch_sub(1, Relaxed);
            }
            blocks[n] = nb;
            frames[n] = nf;
            states[n] = ns;
            n += 1;
        }
        if n == 1 {
            return self.evict(b, f, s);
        }
        for (&blk, &st) in blocks[..n].iter().zip(&states[..n]) {
            if is_mapped(st) {
                if let Err(e) = unsafe { sys::unmap_block(self.block_addr(blk), self.bs) } {
                    fatal("unmap failed", Some(e));
                }
            }
        }
        let bytes = (n * self.bs) as u64;
        self.governor.charge(bytes);
        let ptrs: [*const u8; MAX_RUN] = std::array::from_fn(|i| {
            if i < n {
                self.frame_ptr(frames[i]) as *const u8
            } else {
                ptr::null()
            }
        });
        if let Err(e) = self
            .backing
            .write_blocks((b as u64) << self.shift, self.bs, &ptrs[..n])
        {
            fatal("backing write failed", Some(e));
        }
        self.stats.writebacks.fetch_add(n as u64, Relaxed);
        self.stats.bytes_written.fetch_add(bytes, Relaxed);
        self.stats.evictions.fetch_add(n as u64, Relaxed);
        for &blk in &blocks[..n] {
            self.set_on_disk(blk);
            self.states[blk].store(EMPTY, Release);
        }
        for &nf in &frames[1..n] {
            let fr = &self.frames[nf];
            fr.referenced.store(false, Relaxed);
            fr.hot.store(false, Relaxed);
            fr.block.store(FREE, Release);
            let _ = self.free.push(nf);
        }
    }

    /// Background reclaim (kswapd-style): when the free pool drops below the
    /// low watermark, evict CLOCK victims until it reaches the high one. This
    /// moves unmapping (and its TLB shootdowns) and dirty write-back off the
    /// faulting threads. Returns whether any work was done.
    pub fn reclaim_to_watermark(&self) -> bool {
        if self.free.len() >= self.free_low {
            return false;
        }
        while self.free.len() < self.free_high && !self.shutdown.load(Relaxed) {
            match self.reclaim_one(true) {
                Some(f) => {
                    let fr = &self.frames[f];
                    fr.referenced.store(false, Relaxed);
                    fr.block.store(FREE, Release);
                    let _ = self.free.push(f);
                }
                None => break,
            }
        }
        true
    }

    fn evict(&self, b: usize, f: usize, s: u64) {
        if is_mapped(s) {
            if let Err(e) = unsafe { sys::unmap_block(self.block_addr(b), self.bs) } {
                fatal("unmap failed", Some(e));
            }
        }
        if is_dirty(s) {
            self.write_back(b, f);
        }
        self.stats.evictions.fetch_add(1, Relaxed);
        self.states[b].store(EMPTY, Release);
    }

    fn write_back(&self, b: usize, f: usize) {
        self.governor.charge(self.bs as u64);
        let buf = unsafe { std::slice::from_raw_parts(self.frame_ptr(f), self.bs) };
        if let Err(e) = self.backing.write_block((b as u64) << self.shift, buf) {
            fatal("backing write failed", Some(e));
        }
        self.set_on_disk(b);
        self.stats.writebacks.fetch_add(1, Relaxed);
        self.stats.bytes_written.fetch_add(self.bs as u64, Relaxed);
    }

    /// Sequential-stream detection and readahead.
    fn note_access(&self, b: usize) {
        let depth = self.depth.load(Relaxed) as u64;
        let b64 = b as u64;
        let mut slot = None;
        for i in 0..STREAMS {
            let last = self.stream_last[i].load(Relaxed);
            if b64 > last && b64 <= last + depth + 2 {
                slot = Some(i);
                break;
            }
            if b64 == last {
                return;
            }
        }
        let run = match slot {
            Some(i) => {
                self.stream_last[i].store(b64, Relaxed);
                self.stream_run[i].fetch_add(1, Relaxed) + 1
            }
            None => {
                let i = self.stream_next.fetch_add(1, Relaxed) % STREAMS;
                self.stream_last[i].store(b64, Relaxed);
                self.stream_run[i].store(0, Relaxed);
                0
            }
        };
        if self.policy == Policy::Auto {
            self.auto_policy(slot.is_some());
        }
        if run >= 1 && depth > 0 {
            for k in 1..=depth {
                let nb = b64 + k;
                if nb as usize >= self.nblocks {
                    break;
                }
                if tag(self.states[nb as usize].load(Relaxed)) == EMPTY
                    && self.queue.push(nb).is_err()
                {
                    break;
                }
            }
        }
    }

    /// `Policy::Auto`: switch to the cyclic policy once the sequential
    /// working set (summed over all streams, so N threads sweeping N slices
    /// count) exceeds RAM, and back to plain CLOCK after a burst of random
    /// accesses, e.g. a one-off load scan followed by random lookups.
    fn auto_policy(&self, sequential: bool) {
        // `random_streak` is a score: random accesses add one, stream
        // continuations subtract one. Random access occasionally lands next to
        // a tracked stream, so a strict "consecutive" count would never fire.
        if sequential {
            let mut v = self.random_streak.load(Relaxed);
            while v > 0 {
                match self
                    .random_streak
                    .compare_exchange_weak(v, v - 1, Relaxed, Relaxed)
                {
                    Ok(_) => break,
                    Err(x) => v = x,
                }
            }
            let n = self.seq_accesses.fetch_add(1, Relaxed) + 1;
            if n > self.nframes && !self.cyclic.load(Relaxed) {
                self.cyclic.store(true, Relaxed);
            }
        } else {
            let r = self.random_streak.fetch_add(1, Relaxed) + 1;
            if r > (self.nframes / 16).max(64) {
                self.seq_accesses.store(0, Relaxed);
                if self.cyclic.load(Relaxed) {
                    self.cyclic.store(false, Relaxed);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Explicit interface
    // ------------------------------------------------------------------

    /// Asynchronously load a block if absent (prefetch worker), together with
    /// the following absent blocks, as one vectored read of up to
    /// [`MAX_IO_BYTES`]. Coalescing lets small blocks (precise for random
    /// access) still stream at full SSD bandwidth.
    pub fn prefetch_one(&self, b: usize) {
        let claim = |b: usize| {
            let st = &self.states[b];
            let s = st.load(Acquire);
            tag(s) == EMPTY
                && st
                    .compare_exchange(s, mk(LOADING, 0, false, false), AcqRel, Relaxed)
                    .is_ok()
        };
        if !claim(b) {
            return;
        }
        if !self.on_disk(b) {
            self.stats.prefetch_loads.fetch_add(1, Relaxed);
            let f = self.load(b, false, false, false);
            self.frames[f].referenced.store(true, Relaxed);
            self.frames[f].seq.store(true, Relaxed);
            return;
        }
        // Never read further ahead than asked for.
        let max = self.max_run.min(self.depth.load(Relaxed).max(1));
        let mut blocks = [0usize; MAX_RUN];
        let mut n = 1;
        blocks[0] = b;
        while n < max {
            let nb = b + n;
            if nb >= self.nblocks || !self.on_disk(nb) || !claim(nb) {
                break;
            }
            blocks[n] = nb;
            n += 1;
        }
        let mut frames = [0usize; MAX_RUN];
        for f in frames.iter_mut().take(n) {
            *f = self.alloc_frame();
        }
        let ptrs: [*mut u8; MAX_RUN] = std::array::from_fn(|i| {
            if i < n {
                self.frame_ptr(frames[i])
            } else {
                ptr::null_mut()
            }
        });
        if let Err(e) = self
            .backing
            .read_blocks((b as u64) << self.shift, self.bs, &ptrs[..n])
        {
            fatal("backing read failed", Some(e));
        }
        self.stats
            .bytes_read
            .fetch_add((n * self.bs) as u64, Relaxed);
        self.stats.prefetch_loads.fetch_add(n as u64, Relaxed);
        for i in 0..n {
            // Readahead pages start referenced so CLOCK does not evict them
            // before the stream reaches them.
            self.publish(blocks[i], frames[i], false, false, false, true);
            self.frames[frames[i]].seq.store(true, Relaxed);
        }
    }

    /// Make a block resident and pin it; returns the frame index.
    pub fn pin(&self, b: usize, dirty: bool) -> usize {
        assert!(!(dirty && self.read_only), "write to read-only region");
        let st = &self.states[b];
        let mut spins = 0;
        loop {
            let s = st.load(Acquire);
            match tag(s) {
                RESIDENT => {
                    let f = frame_of(s);
                    let fr = &self.frames[f];
                    fr.pins.fetch_add(1, SeqCst);
                    let s2 = st.load(SeqCst);
                    if matches!(tag(s2), RESIDENT | BUSY) && frame_of(s2) == f {
                        fr.referenced.store(true, Relaxed);
                        if dirty {
                            self.mark_dirty(b);
                        }
                        return f;
                    }
                    fr.pins.fetch_sub(1, SeqCst);
                }
                EMPTY => {
                    if st
                        .compare_exchange(s, mk(LOADING, 0, false, false), AcqRel, Relaxed)
                        .is_ok()
                    {
                        let f = self.load(b, false, false, true);
                        if dirty {
                            self.mark_dirty(b);
                        }
                        return f;
                    }
                }
                _ => self.backoff(&mut spins),
            }
        }
    }

    fn mark_dirty(&self, b: usize) {
        let st = &self.states[b];
        let mut spins = 0;
        loop {
            let s = st.load(Acquire);
            if tag(s) == RESIDENT {
                if is_dirty(s) || st.compare_exchange(s, s | DIRTY, AcqRel, Relaxed).is_ok() {
                    return;
                }
            } else {
                self.backoff(&mut spins);
            }
        }
    }

    pub fn unpin(&self, f: usize) {
        self.frames[f].pins.fetch_sub(1, SeqCst);
    }

    /// Write back a dirty resident block without evicting it.
    pub fn clean(&self, b: usize) -> bool {
        let st = &self.states[b];
        let mut spins = 0;
        loop {
            let s = st.load(Acquire);
            if tag(s) != RESIDENT {
                if matches!(tag(s), BUSY | LOADING) {
                    self.backoff(&mut spins);
                    continue;
                }
                return false;
            }
            if !is_dirty(s) {
                return false;
            }
            let f = frame_of(s);
            if st
                .compare_exchange(s, mk(BUSY, f, true, is_mapped(s)), AcqRel, Relaxed)
                .is_err()
            {
                continue;
            }
            if is_mapped(s) {
                // Write-protect so later writes fault and re-dirty the block.
                if let Err(e) = unsafe { sys::protect_ro(self.block_addr(b), self.bs) } {
                    fatal("mprotect failed", Some(e));
                }
            }
            self.write_back(b, f);
            st.store(mk(RESIDENT, f, false, is_mapped(s)), Release);
            return true;
        }
    }

    /// Evict a block now (writing it back if dirty). Returns false if pinned.
    pub fn evict_block(&self, b: usize) -> bool {
        let st = &self.states[b];
        let mut spins = 0;
        loop {
            let s = st.load(Acquire);
            match tag(s) {
                EMPTY => return true,
                RESIDENT => {
                    let f = frame_of(s);
                    let fr = &self.frames[f];
                    let ev = mk(EVICTING, f, is_dirty(s), is_mapped(s));
                    if st.compare_exchange(s, ev, SeqCst, Relaxed).is_err() {
                        continue;
                    }
                    if fr.pins.load(SeqCst) > 0 {
                        st.store(s, Release);
                        return false;
                    }
                    self.evict(b, f, s);
                    // Return the frame to the free pool.
                    self.lock();
                    if fr.sticky.swap(false, Relaxed) {
                        self.sticky_count.fetch_sub(1, Relaxed);
                    }
                    fr.referenced.store(false, Relaxed);
                    fr.hot.store(false, Relaxed);
                    fr.block.store(FREE, Release);
                    self.unlock();
                    return true;
                }
                _ => self.backoff(&mut spins),
            }
        }
    }

    /// Mark the frames holding these blocks as sticky (kept resident).
    /// Keep the frame holding block `b` resident (`Advice::Hot`).
    pub fn set_hot(&self, b: usize, hot: bool) {
        let s = self.states[b].load(Acquire);
        if tag(s) == RESIDENT {
            self.frames[frame_of(s)].hot.store(hot, Relaxed);
        }
    }

    /// Frames available to streaming data (not held by the cyclic policy).
    pub fn streaming_frames(&self) -> usize {
        if self.cyclic.load(Relaxed) {
            self.nframes - self.sticky_count.load(Relaxed).min(self.nframes)
        } else {
            self.nframes
        }
    }

    pub fn set_cyclic(&self, on: bool) {
        self.cyclic.store(on, Relaxed);
    }

    pub fn resident_blocks(&self) -> usize {
        self.states
            .iter()
            .filter(|s| tag(s.load(Relaxed)) == RESIDENT)
            .count()
    }

    pub fn block_state(&self, b: usize) -> u64 {
        self.states[b].load(Acquire)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Windows placeholders can only be released once their views are
        // unmapped; on Unix this is redundant with the final munmap.
        for b in 0..self.nblocks {
            if is_mapped(self.states[b].load(Acquire)) {
                let _ = unsafe { sys::unmap_block(self.block_addr(b), self.bs) };
            }
        }
        unsafe { sys::release(self.base as *mut u8, self.len, self.bs) };
        sys::destroy_arena(&self.arena);
    }
}

// ----------------------------------------------------------------------
// Global registry used by the process-wide fault handler.
// ----------------------------------------------------------------------

const MAX_REGIONS: usize = 64;
static REGIONS: [AtomicPtr<Inner>; MAX_REGIONS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_REGIONS];
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

fn global_fault(addr: usize, write: Option<bool>) -> bool {
    INFLIGHT.fetch_add(1, SeqCst);
    let mut handled = false;
    for slot in REGIONS.iter() {
        let p = slot.load(SeqCst);
        if p.is_null() {
            continue;
        }
        let inner = unsafe { &*p };
        if inner.contains(addr) {
            handled = inner.handle_fault(addr, write);
            break;
        }
    }
    INFLIGHT.fetch_sub(1, SeqCst);
    handled
}

pub(crate) fn register(inner: *const Inner) -> std::io::Result<()> {
    sys::install_fault_handler(global_fault)?;
    for slot in REGIONS.iter() {
        if slot
            .compare_exchange(ptr::null_mut(), inner as *mut Inner, SeqCst, SeqCst)
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(std::io::Error::other("too many ddram regions"))
}

pub(crate) fn unregister(inner: *const Inner) {
    for slot in REGIONS.iter() {
        let _ = slot.compare_exchange(inner as *mut Inner, ptr::null_mut(), SeqCst, SeqCst);
    }
    // Wait for handlers that may have loaded the pointer to finish.
    while INFLIGHT.load(SeqCst) != 0 {
        sys::yield_now();
    }
}
