// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Benchmarks: ddram vs the kernel's own tiering (file mmap + page cache,
//! anonymous memory + swap) under the same RAM limit.
//!
//! On Linux `--cgroup` puts the process in a cgroup (v1 or v2) whose memory
//! limit equals the RAM budget plus a small slack, so the kernel baselines are
//! held to the same budget ddram enforces internally. Disk traffic is measured
//! from `/proc/diskstats` for the device holding `--dir`, which captures page
//! cache writeback and swap I/O as well as ddram's direct I/O.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ddram::{Config, Policy, Region};

use crate::{parse_size, Args};

const PAGE: usize = 4096;

fn pattern(i: usize) -> u64 {
    (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// YCSB scrambled Zipfian generator (Gray et al., "Quickly generating
/// billion-record synthetic databases").
struct Zipf {
    n: u64,
    theta: f64,
    alpha: f64,
    zetan: f64,
    eta: f64,
}

impl Zipf {
    fn new(n: u64, theta: f64) -> Zipf {
        let zeta = |n: u64| (1..=n).map(|i| 1.0 / (i as f64).powf(theta)).sum::<f64>();
        let zetan = zeta(n);
        let zeta2 = zeta(2);
        Zipf {
            n,
            theta,
            alpha: 1.0 / (1.0 - theta),
            zetan,
            eta: (1.0 - (2.0 / n as f64).powf(1.0 - theta)) / (1.0 - zeta2 / zetan),
        }
    }
    fn sample(&self, u: f64) -> u64 {
        let uz = u * self.zetan;
        let rank = if uz < 1.0 {
            0
        } else if uz < 1.0 + 0.5f64.powf(self.theta) {
            1
        } else {
            ((self.n as f64) * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64
        };
        // Scramble so hot items are spread over the region, not clustered.
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in rank.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h % self.n
    }
}

#[derive(Default, Clone)]
struct Hist {
    buckets: [u64; 32],
    n: u64,
}
impl Hist {
    fn add(&mut self, d: Duration) {
        let ns = d.as_nanos().max(1) as u64;
        // One bucket per power of two of nanoseconds.
        self.buckets[(63 - ns.leading_zeros() as usize).min(31)] += 1;
        self.n += 1;
    }
    fn merge(&mut self, o: &Hist) {
        for i in 0..32 {
            self.buckets[i] += o.buckets[i];
        }
        self.n += o.n;
    }
    fn pct(&self, p: f64) -> u64 {
        let target = (self.n as f64 * p).ceil() as u64;
        let mut acc = 0;
        for (i, &c) in self.buckets.iter().enumerate() {
            acc += c;
            if acc >= target {
                return 1u64 << (i + 1); // upper bound of the bucket
            }
        }
        0
    }
}

#[derive(Clone, Copy, Default)]
struct Disk {
    read: u64,
    written: u64,
}

#[cfg(target_os = "linux")]
fn disk_for(dir: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(dir).ok()?.dev();
    let (maj, min) = (libc::major(dev), libc::minor(dev));
    let real = std::fs::canonicalize(format!("/sys/dev/block/{maj}:{min}")).ok()?;
    let name = if real.join("partition").exists() {
        real.parent()?.file_name()?.to_string_lossy().into_owned()
    } else {
        real.file_name()?.to_string_lossy().into_owned()
    };
    Some(name)
}
#[cfg(not(target_os = "linux"))]
fn disk_for(_dir: &Path) -> Option<String> {
    None
}

fn disk_stats(dev: &Option<String>) -> Disk {
    let Some(dev) = dev else {
        return Disk::default();
    };
    let Ok(s) = std::fs::read_to_string("/proc/diskstats") else {
        return Disk::default();
    };
    for line in s.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() > 9 && f[2] == dev {
            return Disk {
                read: f[5].parse::<u64>().unwrap_or(0) * 512,
                written: f[9].parse::<u64>().unwrap_or(0) * 512,
            };
        }
    }
    Disk::default()
}

// ---------------------------------------------------------------------------
// cgroup memory limit (Linux)
// ---------------------------------------------------------------------------

pub(crate) struct Cgroup {
    path: PathBuf,
    parent_procs: PathBuf,
}

impl Cgroup {
    #[cfg(target_os = "linux")]
    fn enter(limit: u64) -> std::io::Result<Cgroup> {
        let name = format!("ddram-bench-{}", std::process::id());
        let v1 = Path::new("/sys/fs/cgroup/memory");
        let (path, limit_file, parent_procs) = if v1.join("memory.limit_in_bytes").exists() {
            (
                v1.join(&name),
                "memory.limit_in_bytes",
                v1.join("cgroup.procs"),
            )
        } else {
            let v2 = Path::new("/sys/fs/cgroup");
            (v2.join(&name), "memory.max", v2.join("cgroup.procs"))
        };
        std::fs::create_dir_all(&path)?;
        std::fs::write(path.join(limit_file), limit.to_string())?;
        let _ = std::fs::write(path.join("memory.swappiness"), "60");
        std::fs::write(path.join("cgroup.procs"), std::process::id().to_string())?;
        Ok(Cgroup { path, parent_procs })
    }
    #[cfg(not(target_os = "linux"))]
    fn enter(_limit: u64) -> std::io::Result<Cgroup> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "cgroups are Linux-only",
        ))
    }
    fn peak(&self) -> Option<u64> {
        for f in ["memory.max_usage_in_bytes", "memory.peak"] {
            if let Ok(s) = std::fs::read_to_string(self.path.join(f)) {
                return s.trim().parse().ok();
            }
        }
        None
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.parent_procs, std::process::id().to_string());
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// Enter a memory-limited cgroup or exit with an error.
pub(crate) fn enter_cgroup(limit: u64) -> Cgroup {
    Cgroup::enter(limit).unwrap_or_else(|e| {
        eprintln!("cgroup setup failed: {e}");
        std::process::exit(1)
    })
}

// ---------------------------------------------------------------------------
// Engines
// ---------------------------------------------------------------------------

enum Engine {
    Ddram(Region),
    #[cfg(unix)]
    Mmap {
        ptr: *mut u8,
        len: usize,
        path: PathBuf,
    },
}

unsafe impl Sync for Engine {}

impl Engine {
    fn ptr(&self) -> *mut u8 {
        match self {
            Engine::Ddram(r) => r.as_ptr(),
            #[cfg(unix)]
            Engine::Mmap { ptr, .. } => *ptr,
        }
    }
    fn len(&self) -> usize {
        match self {
            Engine::Ddram(r) => r.len(),
            #[cfg(unix)]
            Engine::Mmap { len, .. } => *len,
        }
    }
}

impl Engine {
    fn flush(&self) {
        match self {
            Engine::Ddram(r) => r.flush().expect("flush"),
            #[cfg(unix)]
            Engine::Mmap { ptr, len, path } => unsafe {
                if !path.as_os_str().is_empty() {
                    libc::msync((*ptr).cast(), *len, libc::MS_SYNC);
                }
            },
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Engine::Mmap { ptr, len, path } = self {
            unsafe { libc::munmap((*ptr).cast(), *len) };
            if !path.as_os_str().is_empty() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(unix)]
fn kernel_engine(kind: &str, size: usize, dir: &Path) -> Engine {
    use std::os::unix::io::AsRawFd;
    unsafe {
        if kind == "swap" {
            let p = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                -1,
                0,
            );
            assert!(
                p != libc::MAP_FAILED,
                "mmap anon: {}",
                std::io::Error::last_os_error()
            );
            return Engine::Mmap {
                ptr: p.cast(),
                len: size,
                path: PathBuf::new(),
            };
        }
        let path = dir.join(format!("ddram-bench-mmap-{}.bin", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(size as u64).unwrap();
        let p = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        );
        assert!(
            p != libc::MAP_FAILED,
            "mmap file: {}",
            std::io::Error::last_os_error()
        );
        Engine::Mmap {
            ptr: p.cast(),
            len: size,
            path,
        }
    }
}

#[cfg(not(unix))]
fn kernel_engine(kind: &str, _size: usize, _dir: &Path) -> Engine {
    panic!("engine {kind} is only implemented on Unix");
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

fn fill(p: *mut u64, words: usize, threads: usize) {
    let per = words.div_ceil(threads);
    let pa = p as usize;
    std::thread::scope(|s| {
        for t in 0..threads {
            s.spawn(move || {
                let p = pa as *mut u64;
                let end = ((t + 1) * per).min(words);
                for i in t * per..end {
                    unsafe { p.add(i).write_volatile(pattern(i)) };
                }
            });
        }
    });
}

/// Read one word per cache line, verifying contents.
fn sweep(p: *const u64, words: usize, threads: usize) -> u64 {
    let per = words.div_ceil(threads);
    let pa = p as usize;
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || {
                    let p = pa as *const u64;
                    let end = ((t + 1) * per).min(words);
                    let mut bad = 0u64;
                    let mut i = t * per;
                    while i < end {
                        if unsafe { p.add(i).read_volatile() } != pattern(i) {
                            bad += 1;
                        }
                        i += 8;
                    }
                    bad
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    })
}

fn random_ops(
    p: *mut u64,
    words: usize,
    ops: usize,
    threads: usize,
    zipf: bool,
    write: bool,
) -> (Hist, u64) {
    let pages = (words * 8 / PAGE) as u64;
    let z = zipf.then(|| Zipf::new(pages, 0.99));
    let pa = p as usize;
    let per = ops / threads;
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let z = &z;
                s.spawn(move || {
                    let p = pa as *mut u64;
                    let mut rng = Rng(0x9E37_79B9 ^ ((t as u64 + 1) * 0x1234_5678_9ABC));
                    let mut h = Hist::default();
                    let mut bad = 0;
                    for _ in 0..per {
                        let page = match z {
                            Some(z) => z.sample(rng.unit()),
                            None => rng.next() % pages,
                        };
                        let i =
                            page as usize * (PAGE / 8) + (rng.next() % (PAGE as u64 / 8)) as usize;
                        let t0 = Instant::now();
                        unsafe {
                            if write {
                                p.add(i).write_volatile(pattern(i));
                            } else if p.add(i).read_volatile() != pattern(i) {
                                bad += 1;
                            }
                        }
                        h.add(t0.elapsed());
                    }
                    (h, bad)
                })
            })
            .collect();
        let mut h = Hist::default();
        let mut bad = 0;
        for x in hs {
            let (hh, b) = x.join().unwrap();
            h.merge(&hh);
            bad += b;
        }
        (h, bad)
    })
}

fn gbps(bytes: u64, d: Duration) -> f64 {
    bytes as f64 / d.as_secs_f64() / 1e9
}

#[allow(irrefutable_let_patterns)] // only the ddram engine exists on Windows
pub fn run(a: &Args) {
    let engine_kind = a.get_or("engine", "ddram").to_string();
    let workload = a.get_or("workload", "cyclic").to_string();
    let size = parse_size(a.get_or("size", "8G")) as usize;
    let budget = parse_size(a.get_or("budget", "2G")) as usize;
    let block = parse_size(a.get_or("block", "1M")) as usize;
    let dir = PathBuf::from(a.get_or("dir", "."));
    let passes: usize = a.get_or("passes", "3").parse().unwrap();
    let ops: usize = a.get_or("ops", "200000").parse().unwrap();
    let threads: usize = a.get_or("threads", "1").parse().unwrap();
    let depth: usize = a.get_or("prefetch", "16").parse().unwrap();
    let io_threads: usize = a.get_or("io-threads", "4").parse().unwrap();
    let slack = parse_size(a.get_or("slack", "128M"));
    let policy = match a.get_or("policy", "auto") {
        "clock" => Policy::Clock,
        "cyclic" => Policy::Cyclic,
        _ => Policy::Auto,
    };

    let cg = if a.get("cgroup").is_some() {
        match Cgroup::enter(budget as u64 + slack) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("cgroup setup failed: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let dev = disk_for(&dir);
    let engine = match engine_kind.as_str() {
        "ddram" => {
            let mut c = Config::new(size as u64, budget, &dir);
            c.block_size = block;
            c.prefetch_depth = depth;
            c.io_threads = io_threads;
            c.policy = policy;
            Engine::Ddram(Region::new(c).expect("create region"))
        }
        k @ ("mmap" | "swap") => kernel_engine(k, size, &dir),
        k => panic!("unknown engine {k}"),
    };
    let p = engine.ptr() as *mut u64;
    let words = engine.len() / 8;

    println!(
        "# engine={engine_kind} workload={workload} size={} budget={} block={} threads={threads} dev={}",
        size,
        budget,
        block,
        dev.as_deref().unwrap_or("?")
    );

    let d0 = disk_stats(&dev);
    let t = Instant::now();
    fill(p, words, threads.max(1));
    let fill_t = t.elapsed();
    let d1 = disk_stats(&dev);
    println!(
        "fill        : {:6.2} s  {:6.3} GB/s  disk_w={:.2} GB",
        fill_t.as_secs_f64(),
        gbps(size as u64, fill_t),
        (d1.written - d0.written) as f64 / 1e9
    );
    // Start the measured phase from a clean state on every engine: write back
    // what the fill left dirty (untimed), as a long-running process would
    // have done in the background.
    if a.get("no-settle").is_none() {
        let t = Instant::now();
        engine.flush();
        println!("settle      : {:6.2} s", t.elapsed().as_secs_f64());
    }

    let d0 = disk_stats(&dev);
    let t = Instant::now();
    let mut bad = 0;
    let mut summary = String::new();
    match workload.as_str() {
        "fill" => {}
        "cyclic" | "seq" => {
            for pass in 0..passes {
                let tp = Instant::now();
                let ds = disk_stats(&dev);
                bad += sweep(p, words, threads.max(1));
                let el = tp.elapsed();
                let de = disk_stats(&dev);
                println!(
                    "pass {pass:<6} : {:6.2} s  {:6.3} GB/s  disk_r={:.2} GB",
                    el.as_secs_f64(),
                    gbps(size as u64, el),
                    (de.read - ds.read) as f64 / 1e9
                );
            }
            let el = t.elapsed();
            summary = format!("throughput_gbps={:.3}", gbps((size * passes) as u64, el));
        }
        "random" | "zipf" | "zipf-write" => {
            let (h, b) = random_ops(
                p,
                words,
                ops,
                threads.max(1),
                workload != "random",
                workload == "zipf-write",
            );
            bad += b;
            let el = t.elapsed();
            println!(
                "ops         : {} in {:.2} s = {:.0} ops/s  p50<={}us p99<={}us p99.9<={}us",
                h.n,
                el.as_secs_f64(),
                h.n as f64 / el.as_secs_f64(),
                h.pct(0.5) / 1000,
                h.pct(0.99) / 1000,
                h.pct(0.999) / 1000
            );
            summary = format!(
                "ops_per_s={:.0} p50_us={} p99_us={} p999_us={}",
                h.n as f64 / el.as_secs_f64(),
                h.pct(0.5) / 1000,
                h.pct(0.99) / 1000,
                h.pct(0.999) / 1000
            );
        }
        w => panic!("unknown workload {w}"),
    }
    let el = t.elapsed();
    let d1 = disk_stats(&dev);
    if let Engine::Ddram(r) = &engine {
        let s = r.stats();
        println!(
            "ddram       : hard={} soft={} wfault={} prefetch={} evict={} writeback={} direct_reclaim={} cyclic={} direct_io={}",
            s.hard_faults, s.soft_faults, s.write_faults, s.prefetch_loads, s.evictions, s.writebacks, s.direct_reclaims, s.cyclic,
            r.direct_io()
        );
    }
    let peak = cg.as_ref().and_then(|c| c.peak()).unwrap_or(0);
    println!(
        "RESULT engine={engine_kind} workload={workload} secs={:.3} disk_read_gb={:.3} disk_write_gb={:.3} peak_mem_mb={} errors={bad} {summary}",
        el.as_secs_f64(),
        (d1.read - d0.read) as f64 / 1e9,
        (d1.written - d0.written) as f64 / 1e9,
        peak >> 20
    );
    drop(engine);
    drop(cg);
    if bad != 0 {
        eprintln!("DATA VERIFICATION FAILED: {bad} mismatches");
        std::process::exit(1);
    }
}
