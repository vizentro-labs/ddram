// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! # ddram — extend DRAM with NVMe/SSD capacity
//!
//! `ddram` gives a process a memory region larger than the RAM it is allowed
//! to use. Hot blocks live in a fixed-size RAM arena; cold blocks live in a
//! file on an SSD. It works on Linux, macOS and Windows.
//!
//! Two ways to use a [`Region`]:
//!
//! * **Transparent**: [`Region::as_ptr`] returns an ordinary pointer. Touching
//!   a non-resident block faults; the handler brings it in from the SSD,
//!   evicting a cold block if the budget is full. Existing code (matrix
//!   kernels, asset parsers, hash tables) runs unmodified.
//! * **Explicit**: [`Region::pin`], [`Region::read_at`], [`Region::write_at`],
//!   [`Region::prefetch`] and [`Region::advise`] for engines that know their
//!   access pattern (LLM layer streaming, texture streaming, KV caches). Pinned
//!   blocks live in the arena, which can be registered once with a GPU API
//!   (CUDA host registration / cuFile, Metal no-copy buffers, DirectStorage)
//!   so uploads are DMA from a fixed staging area. See [`Region::arena`].
//! * **Planned**: [`Region::set_plan`] installs a [`Plan`], the order in which
//!   the application will touch the region (a list of ranges, optionally
//!   cyclic). The pager then prefetches exactly what comes next and evicts by
//!   Belady's optimal rule instead of guessing from past accesses. See
//!   [`plan`].
//!
//! NAND is not DRAM: a 4 KiB read costs ~50-100 µs vs ~0.1 µs. ddram hides
//! that with large blocks, parallel readahead and a scan-resistant policy, and
//! limits NAND wear by writing back only dirty blocks, at a rate the
//! [`governor::WriteGovernor`] can cap from the drive's SMART endurance data.

pub mod backing;
pub mod ffi;
pub mod governor;
mod pager;
pub mod plan;
pub mod smart;
mod sys;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub use pager::Policy;
pub use plan::Plan;

use backing::Backing;
use governor::WriteGovernor;
use pager::{Inner, InnerParams};

/// Region configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Size of the region in bytes (rounded up to the block size). Ignored
    /// for [`Config::read_only`] regions, which take the file's size.
    pub size: u64,
    /// Bytes of RAM the region may use for resident blocks.
    pub ram_budget: usize,
    /// Paging unit. Power of two, at least the OS mapping granularity (4 KiB
    /// Linux/x86 macOS, 16 KiB Apple Silicon, 64 KiB Windows). Larger blocks
    /// amortize SSD latency; default 1 MiB.
    pub block_size: usize,
    /// Backing file, or a directory in which a temporary file is created
    /// (and deleted when the region is dropped).
    pub backing: PathBuf,
    /// Map an existing file read-only (model weights, game assets).
    pub read_only: bool,
    /// Bypass the OS page cache (O_DIRECT / F_NOCACHE / NO_BUFFERING).
    pub direct_io: bool,
    /// Blocks to read ahead once a sequential stream is detected.
    pub prefetch_depth: usize,
    /// Background readahead threads (each keeps one SSD request in flight).
    pub io_threads: usize,
    /// Cap on write-back bandwidth in bytes/s (0 = unlimited).
    pub write_rate_limit: u64,
    pub policy: Policy,
    /// Write back dirty blocks when the region is dropped.
    pub persist: bool,
}

impl Config {
    pub fn new(size: u64, ram_budget: usize, backing: impl Into<PathBuf>) -> Config {
        Config {
            size,
            ram_budget,
            block_size: 1 << 20,
            backing: backing.into(),
            read_only: false,
            direct_io: true,
            prefetch_depth: 16,
            io_threads: 4,
            write_rate_limit: 0,
            policy: Policy::Auto,
            persist: false,
        }
    }

    /// Map an existing file read-only.
    pub fn read_only_file(path: impl Into<PathBuf>, ram_budget: usize) -> Config {
        let mut c = Config::new(0, ram_budget, path);
        c.read_only = true;
        c
    }
}

/// Access hints, mirroring `madvise` plus GPU-workload specific ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advice {
    /// Default CLOCK behaviour, readahead on.
    Normal,
    /// Random access: disable readahead.
    Random,
    /// Sequential access: aggressive readahead.
    Sequential,
    /// Repeated sweeps over more data than fits in RAM (e.g. LLM weights).
    Cyclic,
    /// Load these bytes in the background now.
    WillNeed,
    /// Write back if dirty and drop from RAM; data is preserved.
    Cold,
    /// Keep resident as long as possible (e.g. embeddings, UI textures).
    Hot,
}

/// How [`Region::apply_health`] turns a drive health log into limits.
#[derive(Clone, Debug)]
pub struct HealthPolicy {
    /// Share of the drive's sustainable daily write budget ddram may use.
    pub write_share: f64,
    /// Datasheet endurance in bytes (TBW), if known.
    pub rated_tbw: Option<f64>,
    /// Endurance assumed when there is no rating and the drive reports < 1% used.
    pub fallback_tbw: f64,
    /// Planned service life in years (typically the warranty).
    pub service_years: f64,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        HealthPolicy {
            write_share: 0.25,
            rated_tbw: None,
            fallback_tbw: 150e12,
            service_years: 5.0,
        }
    }
}

/// Outcome of [`Region::apply_health`].
#[derive(Clone, Debug, PartialEq)]
pub enum HealthStatus {
    /// Write-back capped at `write_rate` bytes/s.
    Ok { write_rate: u64 },
    /// Temperature warning: readahead depth halved, writes capped as for `Ok`.
    Hot { write_rate: u64 },
    /// Critical warning (spare exhausted, degraded, read-only...): write-back
    /// clamped to a trickle. Move the backing file to another drive.
    Unsafe { warnings: Vec<&'static str> },
}

/// Counters for observability and benchmarking.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Faults that required SSD I/O (or zero-fill) on the faulting thread.
    pub hard_faults: u64,
    /// Faults on blocks already brought in by readahead (no I/O wait).
    pub soft_faults: u64,
    /// First write to a clean block.
    pub write_faults: u64,
    pub loads: u64,
    pub prefetch_loads: u64,
    pub zero_fills: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub evictions: u64,
    pub writebacks: u64,
    pub resident_blocks: u64,
    pub throttled_ns: u64,
    /// Frame allocations that had to evict on the faulting thread because
    /// the background reclaimer's free pool was empty.
    pub direct_reclaims: u64,
    pub cyclic: bool,
    /// Whether an access plan is installed.
    pub plan_active: bool,
    /// Faults on blocks the plan covers / does not cover.
    pub plan_hits: u64,
    pub plan_misses: u64,
    /// Ranges of the plan consumed so far (cumulative over cycles).
    pub plan_position: u64,
}

/// A tiered memory region.
pub struct Region {
    inner: Arc<Inner>,
    workers: Vec<JoinHandle<()>>,
    temp_file: Option<PathBuf>,
    persist: bool,
}

/// Minimum block size accepted on this OS.
pub fn min_block_size() -> usize {
    sys::map_granularity()
}

/// Each resident block can be a separate kernel mapping; on Linux their
/// number is capped by `vm.max_map_count`. Fail early with a clear message
/// instead of aborting in the fault handler later.
fn check_map_count(nframes: usize) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/sys/vm/max_map_count") {
        let max: usize = s.trim().parse().unwrap_or(usize::MAX);
        // Mapped frames plus the PROT_NONE gaps between them, plus headroom
        // for the rest of the process.
        let need = 2 * nframes + 4096;
        if need > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{nframes} frames need vm.max_map_count >= {need} (currently {max}); \
                     raise it (sysctl -w vm.max_map_count={}) or use a larger block_size",
                    need.next_power_of_two()
                ),
            ));
        }
    }
    let _ = nframes;
    Ok(())
}

fn temp_path(dir: &Path) -> PathBuf {
    use std::sync::atomic::AtomicUsize;
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    dir.join(format!(
        "ddram-{}-{}.swap",
        std::process::id(),
        SEQ.fetch_add(1, Relaxed)
    ))
}

impl Region {
    pub fn new(cfg: Config) -> io::Result<Region> {
        let gran = sys::map_granularity();
        let bs = cfg.block_size;
        if !bs.is_power_of_two() || bs < gran {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("block_size must be a power of two >= {gran}"),
            ));
        }
        let nframes = cfg.ram_budget / bs;
        if nframes < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ram_budget must hold at least 4 blocks",
            ));
        }
        check_map_count(nframes)?;
        let mut temp_file = None;
        let backing = if cfg.read_only {
            Backing::open_read_only(&cfg.backing, cfg.direct_io)?
        } else {
            let len = cfg.size.div_ceil(bs as u64) * bs as u64;
            let path = if cfg.backing.is_dir() {
                let p = temp_path(&cfg.backing);
                temp_file = Some(p.clone());
                p
            } else {
                cfg.backing.clone()
            };
            Backing::open(&path, len, cfg.direct_io)?
        };
        let len = (backing.len.div_ceil(bs as u64) * bs as u64) as usize;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty region"));
        }
        let existing = cfg.read_only || (temp_file.is_none() && cfg.persist);
        let inner = Arc::new(Inner::new(InnerParams {
            len,
            bs,
            nframes: nframes.min(len / bs).max(4),
            backing,
            governor: WriteGovernor::new(cfg.write_rate_limit, (bs as u64) * 4),
            depth: cfg.prefetch_depth,
            io_threads: cfg.io_threads,
            policy: cfg.policy,
            existing_data: existing,
        })?);
        pager::register(Arc::as_ptr(&inner))?;
        let mut workers: Vec<_> = (0..cfg.io_threads)
            .map(|i| {
                let inner = inner.clone();
                std::thread::Builder::new()
                    .name(format!("ddram-io-{i}"))
                    .spawn(move || prefetch_worker(inner))
                    .expect("spawn ddram io thread")
            })
            .collect();
        let r = inner.clone();
        workers.push(
            std::thread::Builder::new()
                .name("ddram-reclaim".into())
                .spawn(move || reclaim_worker(r))
                .expect("spawn ddram reclaim thread"),
        );
        Ok(Region {
            inner,
            workers,
            temp_file,
            persist: cfg.persist,
        })
    }

    /// Base of the transparent view. Valid for [`Region::len`] bytes while the
    /// region is alive.
    pub fn as_ptr(&self) -> *mut u8 {
        self.inner.base as *mut u8
    }

    pub fn len(&self) -> usize {
        self.inner.len
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len == 0
    }

    pub fn block_size(&self) -> usize {
        self.inner.bs
    }

    pub fn ram_budget(&self) -> usize {
        self.inner.nframes * self.inner.bs
    }

    /// The RAM arena: register `(ptr, len)` once with a GPU/DMA API
    /// (`cudaHostRegister`, `cuFileBufRegister`, Metal
    /// `newBufferWithBytesNoCopy`, a DirectStorage destination) so that
    /// pinned blocks can be uploaded without an extra copy.
    pub fn arena(&self) -> (*mut u8, usize) {
        (self.inner.arena.ptr, self.inner.arena.len)
    }

    /// Pin block `index` in RAM, returning a guard over its bytes.
    pub fn pin(&self, index: usize) -> Pin<'_> {
        assert!(index < self.inner.nblocks);
        let frame = self.inner.pin(index, false);
        Pin {
            region: self,
            frame,
            block: index,
        }
    }

    /// Pin block `index` for writing (marks it dirty).
    pub fn pin_mut(&self, index: usize) -> PinMut<'_> {
        assert!(index < self.inner.nblocks);
        let frame = self.inner.pin(index, true);
        PinMut {
            region: self,
            frame,
            block: index,
        }
    }

    /// Release a pin taken through the C API (which forgets the guard).
    pub(crate) fn unpin_block(&self, index: usize) {
        let s = self.inner.block_state(index);
        self.inner.unpin((s >> 32) as usize);
    }

    pub fn read_at(&self, mut off: usize, mut buf: &mut [u8]) {
        assert!(off + buf.len() <= self.len());
        let bs = self.inner.bs;
        while !buf.is_empty() {
            let b = off / bs;
            let o = off % bs;
            let n = (bs - o).min(buf.len());
            let p = self.pin(b);
            buf[..n].copy_from_slice(&p[o..o + n]);
            buf = &mut buf[n..];
            off += n;
        }
    }

    pub fn write_at(&self, mut off: usize, mut buf: &[u8]) {
        assert!(off + buf.len() <= self.len());
        let bs = self.inner.bs;
        while !buf.is_empty() {
            let b = off / bs;
            let o = off % bs;
            let n = (bs - o).min(buf.len());
            let mut p = self.pin_mut(b);
            p[o..o + n].copy_from_slice(&buf[..n]);
            buf = &buf[n..];
            off += n;
        }
    }

    fn block_range(&self, off: usize, len: usize) -> std::ops::Range<usize> {
        let bs = self.inner.bs;
        let end = (off + len).min(self.len());
        off / bs..end.div_ceil(bs)
    }

    /// Queue background loads for `[off, off+len)`.
    ///
    /// This is a hint: at most half of the frames not held by the cyclic
    /// policy are requested, and requests beyond the queue's capacity are
    /// dropped, so prefetching more than fits in RAM never evicts data that
    /// was prefetched earlier but not yet used.
    pub fn prefetch(&self, off: usize, len: usize) {
        let limit = (self.inner.streaming_frames() / 2).max(1);
        for b in self.block_range(off, len).take(limit) {
            if self.inner.queue.push(b as u64).is_err() {
                break;
            }
        }
    }

    pub fn advise(&self, off: usize, len: usize, advice: Advice) {
        let inner = &self.inner;
        match advice {
            Advice::Normal => {
                inner.depth.store(16, Relaxed);
                inner.set_cyclic(false);
            }
            Advice::Random => inner.depth.store(0, Relaxed),
            Advice::Sequential => inner.depth.store(64, Relaxed),
            Advice::Cyclic => inner.set_cyclic(true),
            Advice::WillNeed => self.prefetch(off, len),
            Advice::Cold => {
                for b in self.block_range(off, len) {
                    inner.evict_block(b);
                }
            }
            Advice::Hot => {
                for b in self.block_range(off, len) {
                    let p = self.pin(b);
                    inner.set_hot(b, true);
                    drop(p);
                }
            }
        }
    }

    /// Install an access plan: the order in which the application will touch
    /// the region. From now on the pager prefetches along the plan and evicts
    /// the block whose next planned use is furthest away (Belady's optimal
    /// policy). Heuristic readahead and the cyclic policy are off while a
    /// plan is installed; explicit `prefetch`/`advise` still work. Replaces
    /// any previous plan; safe to call while other threads access the region.
    pub fn set_plan(&self, plan: &Plan) -> io::Result<()> {
        self.inner.set_plan(Some(plan))
    }

    /// Remove the access plan and return to the configured [`Policy`].
    pub fn clear_plan(&self) {
        let _ = self.inner.set_plan(None);
    }

    /// Start recording the order in which blocks are brought in (every
    /// fault, i.e. the first touch of a block that is not mapped, and every
    /// explicit pin or read/write of an absent block), keeping up to
    /// `max_entries`. Recording costs one atomic store per fault. Use
    /// [`Region::recorded_plan`] to turn the trace into a [`Plan`], which can
    /// be installed with [`Region::set_plan`] and saved for the next run.
    pub fn record(&self, max_entries: usize) {
        self.inner.set_recorder(Some(max_entries));
    }

    /// Stop recording and return the trace (block indices in fault order).
    pub fn stop_recording(&self) -> Vec<u64> {
        self.inner.set_recorder(None).unwrap_or_default()
    }

    /// The trace recorded so far and whether the buffer is full, without
    /// stopping; `None` if not recording.
    pub fn recorded(&self) -> Option<(Vec<u64>, bool)> {
        self.inner.recorded()
    }

    /// The recorded trace as a plan: one pass over everything recorded so
    /// far (see [`Plan::from_trace`]). `None` if not recording or nothing
    /// was recorded.
    pub fn recorded_plan(&self, cyclic: bool) -> Option<Plan> {
        let (blocks, _) = self.inner.recorded()?;
        if blocks.is_empty() {
            return None;
        }
        Some(Plan::from_trace(&blocks, self.inner.bs as u64, cyclic))
    }

    /// Write back all dirty blocks, wait for write-backs already in flight,
    /// and sync the backing file.
    pub fn flush(&self) -> io::Result<()> {
        for b in 0..self.inner.nblocks {
            self.inner.clean(b);
        }
        self.inner.backing.sync()
    }

    /// Set the write-back cap in bytes/s (0 = unlimited).
    pub fn set_write_rate_limit(&self, bytes_per_sec: u64) {
        self.inner
            .governor
            .set_rate(bytes_per_sec, self.inner.bs as u64 * 4);
    }

    /// Apply NVMe health data (see [`smart`]) to this region: cap write-back
    /// at a share of the drive's remaining endurance budget, back off
    /// readahead on a temperature warning, and clamp writes on a drive that
    /// reports critical warnings. Call it periodically (e.g. hourly) with a
    /// fresh log; [`Region::refresh_health`] does the device lookup and read.
    pub fn apply_health(&self, h: &smart::HealthLog, p: &HealthPolicy) -> HealthStatus {
        if h.is_unsafe_for_tiering() {
            self.set_write_rate_limit(1 << 20);
            return HealthStatus::Unsafe {
                warnings: h.warnings(),
            };
        }
        let budget =
            smart::EnduranceBudget::from_health(h, p.rated_tbw, p.fallback_tbw, p.service_years);
        let rate = budget.rate_for_share(p.write_share).max(1 << 20);
        self.set_write_rate_limit(rate);
        if h.critical_warning & smart::critical::TEMPERATURE != 0 {
            let d = self.inner.depth.load(Relaxed);
            self.inner.depth.store((d / 2).max(1), Relaxed);
            return HealthStatus::Hot { write_rate: rate };
        }
        HealthStatus::Ok { write_rate: rate }
    }

    /// Read the health log of the drive holding the backing file and apply
    /// it. Needs privileges on Linux (CAP_SYS_ADMIN).
    pub fn refresh_health(&self, p: &HealthPolicy) -> io::Result<HealthStatus> {
        let dev = smart::device_for_path(self.backing_path())?;
        let h = smart::read_health(&dev)?;
        Ok(self.apply_health(&h, p))
    }

    pub fn stats(&self) -> Stats {
        let c = &self.inner.stats;
        let ps = self.inner.plan_stats();
        Stats {
            hard_faults: c.hard_faults.load(Relaxed),
            soft_faults: c.soft_faults.load(Relaxed),
            write_faults: c.write_faults.load(Relaxed),
            loads: c.loads.load(Relaxed),
            prefetch_loads: c.prefetch_loads.load(Relaxed),
            zero_fills: c.zero_fills.load(Relaxed),
            bytes_read: c.bytes_read.load(Relaxed),
            bytes_written: c.bytes_written.load(Relaxed),
            evictions: c.evictions.load(Relaxed),
            writebacks: c.writebacks.load(Relaxed),
            resident_blocks: self.inner.resident_blocks() as u64,
            throttled_ns: self.inner.governor.throttled_ns.load(Relaxed),
            direct_reclaims: c.direct_reclaims.load(Relaxed),
            cyclic: self.inner.is_cyclic(),
            plan_active: ps.is_some(),
            plan_hits: ps.map_or(0, |p| p.0),
            plan_misses: ps.map_or(0, |p| p.1),
            plan_position: ps.map_or(0, |p| p.2),
        }
    }

    pub fn backing_path(&self) -> &Path {
        &self.inner.backing.path
    }

    pub fn direct_io(&self) -> bool {
        self.inner.backing.direct
    }
}

fn prefetch_worker(inner: Arc<Inner>) {
    let mut idle = 0u32;
    while !inner.shutdown.load(Relaxed) {
        match inner.queue.pop() {
            Some(b) => {
                idle = 0;
                inner.prefetch_one(b as usize);
            }
            None => {
                idle = (idle + 1).min(10);
                std::thread::sleep(Duration::from_micros(10u64 << idle.min(6)));
            }
        }
    }
}

fn reclaim_worker(inner: Arc<Inner>) {
    let mut idle = 0u32;
    while !inner.shutdown.load(Relaxed) {
        let planned = inner.plan_tick();
        if inner.reclaim_to_watermark() || planned {
            idle = 0;
        } else {
            idle = (idle + 1).min(6);
            std::thread::sleep(Duration::from_micros(20u64 << idle));
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Relaxed);
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        if self.persist && !self.inner.read_only {
            let _ = self.flush();
        }
        pager::unregister(Arc::as_ptr(&self.inner));
        if let Some(p) = self.temp_file.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A pinned, read-only block.
pub struct Pin<'a> {
    region: &'a Region,
    frame: usize,
    block: usize,
}

impl Pin<'_> {
    pub fn block(&self) -> usize {
        self.block
    }
    /// Byte offset of this block inside [`Region::arena`] (for GPU uploads).
    pub fn arena_offset(&self) -> usize {
        self.frame * self.region.inner.bs
    }
}

impl std::ops::Deref for Pin<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        let i = &self.region.inner;
        unsafe { std::slice::from_raw_parts(i.frame_ptr(self.frame), i.bs) }
    }
}

impl Drop for Pin<'_> {
    fn drop(&mut self) {
        self.region.inner.unpin(self.frame);
    }
}

/// A pinned, writable block.
pub struct PinMut<'a> {
    region: &'a Region,
    frame: usize,
    block: usize,
}

impl PinMut<'_> {
    pub fn block(&self) -> usize {
        self.block
    }
    pub fn arena_offset(&self) -> usize {
        self.frame * self.region.inner.bs
    }
}

impl std::ops::Deref for PinMut<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        let i = &self.region.inner;
        unsafe { std::slice::from_raw_parts(i.frame_ptr(self.frame), i.bs) }
    }
}

impl std::ops::DerefMut for PinMut<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        let i = &self.region.inner;
        unsafe { std::slice::from_raw_parts_mut(i.frame_ptr(self.frame), i.bs) }
    }
}

impl Drop for PinMut<'_> {
    fn drop(&mut self) {
        self.region.inner.unpin(self.frame);
    }
}
