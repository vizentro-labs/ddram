// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! `ddram llm`: a synthetic LLM decode loop that streams weights from a file.
//!
//! Each "token" runs a matrix-vector product through every layer, which means
//! reading every weight once per token, as batch-1 transformer decoding does.
//! With `--experts N --active K` each layer holds N expert matrices of which
//! K are used per token (mixture-of-experts), chosen by a deterministic,
//! skewed router so every engine computes the same thing.
//!
//! The model file is mapped read-only through ddram or through the kernel
//! (`mmap` + `MADV_SEQUENTIAL`/`MADV_WILLNEED`, i.e. what llama.cpp does by
//! default), under the same memory limit. Outputs carry a checksum so all
//! engines are proven to compute identical results.
//!
//! Engines:
//! * `ddram`: cyclic policy plus `WillNeed` hints one layer ahead (v0.1
//!   behaviour).
//! * `ddram` with `--plan`: an access plan. Dense models declare one cyclic
//!   plan over the layers; MoE models declare each token's expert sequence
//!   when the token starts (what a pre-gating router provides). The pager
//!   then prefetches exactly that and evicts by Belady's rule.
//! * `mmap`: the kernel page cache with `MADV_WILLNEED` on the same ranges
//!   the ddram engine hints, one layer ahead.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ddram::{Advice, Config, Plan, Policy, Region};

use crate::bench::{disk_for, disk_stats};
use crate::{parse_size, Args};

const D: usize = 4096; // hidden size

fn gen_model(path: &Path, bytes: u64) {
    if std::fs::metadata(path)
        .map(|m| m.len() == bytes)
        .unwrap_or(false)
    {
        return;
    }
    eprintln!(
        "generating {:.1} GB model file at {} ...",
        bytes as f64 / 1e9,
        path.display()
    );
    let mut f = std::io::BufWriter::with_capacity(8 << 20, std::fs::File::create(path).unwrap());
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut buf = vec![0u8; 1 << 20];
    let mut left = bytes;
    while left > 0 {
        for c in buf.chunks_exact_mut(4) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Small weights in [-1/64, 1/64).
            let v = ((x >> 40) as f32 / (1u64 << 24) as f32 - 0.5) / 32.0;
            c.copy_from_slice(&v.to_le_bytes());
        }
        let n = left.min(buf.len() as u64) as usize;
        f.write_all(&buf[..n]).unwrap();
        left -= n as u64;
    }
    f.flush().unwrap();
    f.get_ref().sync_all().unwrap();
}

/// y += W x for a [rows x D] row-major block, split across threads.
fn matvec(w: &[f32], x: &[f32], y: &mut [f32], threads: usize) {
    let rows = y.len();
    let per = rows.div_ceil(threads);
    std::thread::scope(|s| {
        for (t, yc) in y.chunks_mut(per).enumerate() {
            let w = &w[t * per * D..];
            s.spawn(move || {
                for (r, out) in yc.iter_mut().enumerate() {
                    let row = &w[r * D..(r + 1) * D];
                    // 8 accumulators so the compiler vectorizes.
                    let mut acc = [0f32; 8];
                    for (a, b) in row.chunks_exact(8).zip(x.chunks_exact(8)) {
                        for i in 0..8 {
                            acc[i] += a[i] * b[i];
                        }
                    }
                    *out += acc.iter().sum::<f32>();
                }
            });
        }
    });
}

/// Deterministic skewed router: expert `e` is drawn with probability
/// proportional to 1/(e+1)^skew; `active` distinct experts per (token, layer).
struct Router {
    cdf: Vec<f64>,
    active: usize,
}

impl Router {
    fn new(experts: usize, active: usize, skew: f64) -> Router {
        let w: Vec<f64> = (0..experts)
            .map(|e| 1.0 / ((e + 1) as f64).powf(skew))
            .collect();
        let total: f64 = w.iter().sum();
        let mut acc = 0.0;
        let cdf = w
            .iter()
            .map(|v| {
                acc += v / total;
                acc
            })
            .collect();
        Router {
            cdf,
            active: active.min(experts),
        }
    }

    fn pick(&self, token: usize, layer: usize) -> Vec<usize> {
        let mut x = (token as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (layer as u64 + 7).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        let mut out = Vec::with_capacity(self.active);
        while out.len() < self.active {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let u = (x >> 11) as f64 / (1u64 << 53) as f64;
            let e = self
                .cdf
                .iter()
                .position(|&c| u < c)
                .unwrap_or(self.cdf.len() - 1);
            if !out.contains(&e) {
                out.push(e);
            }
        }
        out.sort_unstable();
        out
    }
}

pub fn run(a: &Args) {
    let size = parse_size(a.get_or("model-size", "8G"));
    let budget = parse_size(a.get_or("budget", "2G")) as usize;
    let layers: usize = a.get_or("layers", "32").parse().unwrap();
    let tokens: usize = a.get_or("tokens", "4").parse().unwrap();
    let threads: usize = a.get_or("threads", "4").parse().unwrap();
    let experts: usize = a.get_or("experts", "1").parse().unwrap();
    let active: usize = a.get_or("active", "1").parse().unwrap();
    let skew: f64 = a.get_or("expert-skew", "1.0").parse().unwrap();
    let use_plan = a.get("plan").is_some();
    let dir = PathBuf::from(a.get_or("dir", "."));
    let engine = a.get_or("engine", "ddram").to_string();
    let block = parse_size(a.get_or("block", "1M")) as usize;
    let dev = disk_for(&dir);

    // Rows per expert matrix: a multiple of 8 so the block holds whole rows.
    let expert_rows = (size as usize / 4 / layers / experts / D) & !7;
    let expert_floats = expert_rows * D;
    let expert_bytes = expert_floats * 4;
    let layer_bytes = expert_bytes * experts;
    let model_bytes = (layer_bytes * layers) as u64;
    let path = dir.join(format!("ddram-llm-{model_bytes}.weights"));
    gen_model(&path, model_bytes);
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
    let router = Router::new(experts, active, skew);
    let expert_off = |l: usize, e: usize| l * layer_bytes + e * expert_bytes;

    let _cg = if a.get("cgroup").is_some() {
        Some(crate::bench::enter_cgroup(
            budget as u64 + parse_size(a.get_or("slack", "128M")),
        ))
    } else {
        None
    };

    // Map the weights.
    let region = (engine == "ddram").then(|| {
        let mut c = Config::read_only_file(&path, budget);
        c.block_size = block;
        c.policy = if use_plan {
            Policy::Clock
        } else {
            Policy::Cyclic
        };
        c.prefetch_depth = 32;
        Region::new(c).expect("region")
    });
    if let (Some(r), true, 1) = (&region, use_plan, experts) {
        // Dense: one cyclic plan over the layers, declared once.
        let mut plan = Plan::cyclic();
        for l in 0..layers {
            plan.push(expert_off(l, 0) as u64, layer_bytes as u64);
        }
        r.set_plan(&plan).expect("plan");
    }
    #[cfg(unix)]
    let mapping: Option<(*mut libc::c_void, usize)> = (engine == "mmap").then(|| unsafe {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(&path).unwrap();
        let len = model_bytes as usize;
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            f.as_raw_fd(),
            0,
        );
        assert!(p != libc::MAP_FAILED);
        libc::madvise(p, len, libc::MADV_SEQUENTIAL);
        (p, len)
    });
    #[cfg(not(unix))]
    let mapping: Option<(*mut u8, usize)> = None;
    let base: *const f32 = match (&region, mapping) {
        (Some(r), _) => r.as_ptr() as *const f32,
        (None, Some((p, _))) => p as *const f32,
        _ => panic!("unknown engine {engine}"),
    };

    let mode = if use_plan { "plan" } else { "hints" };
    println!(
        "# llm engine={engine} mode={mode} model={:.2} GB layers={layers} experts={experts} active={active} skew={skew} rows/expert={expert_rows} hidden={D} budget={:.2} GB threads={threads}",
        model_bytes as f64 / 1e9,
        budget as f64 / 1e9
    );

    // Hint a set of expert ranges (one layer ahead) on the baseline engines.
    let hint = |ranges: &[(usize, usize)]| {
        for &(off, len) in ranges {
            match (&region, engine.as_str()) {
                (Some(r), _) => r.advise(off, len, Advice::WillNeed),
                #[cfg(unix)]
                (None, "mmap") => unsafe {
                    let (p, _) = mapping.unwrap();
                    libc::madvise((p as *mut u8).add(off).cast(), len, libc::MADV_WILLNEED);
                },
                _ => {}
            }
        }
    };

    let mut x = vec![1.0f32 / D as f32; D];
    let mut y = vec![0f32; expert_rows];
    let t_all = Instant::now();
    let mut checksum = 0f64;
    let mut steady = 0f64;
    let d0 = disk_stats(&dev);
    let mut read0 = region.as_ref().map_or(0, |r| r.stats().bytes_read);
    for tok in 0..tokens {
        let t = Instant::now();
        // The token's expert sequence (what a pre-gating router knows).
        let picks: Vec<Vec<usize>> = (0..layers).map(|l| router.pick(tok, l)).collect();
        let ranges = |l: usize| -> Vec<(usize, usize)> {
            picks[l]
                .iter()
                .map(|&e| (expert_off(l, e), expert_bytes))
                .collect()
        };
        if let (Some(r), true, true) = (&region, use_plan, experts > 1) {
            let mut plan = Plan::new();
            for l in 0..layers {
                for (off, len) in ranges(l) {
                    plan.push(off as u64, len as u64);
                }
            }
            r.set_plan(&plan).expect("plan");
        }
        for l in 0..layers {
            if !use_plan {
                // Hint the next layer so I/O overlaps this layer's compute.
                let next = if l + 1 < layers {
                    ranges(l + 1)
                } else {
                    // Next token: hint its first layer (dense: same every token).
                    (0..1)
                        .flat_map(|_| router.pick(tok + 1, 0))
                        .map(|e| (expert_off(0, e), expert_bytes))
                        .collect()
                };
                hint(&next);
            }
            y.iter_mut().for_each(|v| *v = 0.0);
            for (off, _) in ranges(l) {
                let w = unsafe { std::slice::from_raw_parts(base.add(off / 4), expert_floats) };
                matvec(w, &x, &mut y, threads);
            }
            // Fold rows back to the hidden size and normalize.
            let mut nx = vec![0f32; D];
            for (i, v) in y.iter().enumerate() {
                nx[i % D] += v;
            }
            let norm = nx.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
            for (xi, v) in x.iter_mut().zip(&nx) {
                *xi = v / norm;
            }
        }
        let tok_sum: f64 = x.iter().map(|&v| v as f64).sum();
        checksum += tok_sum;
        let el = t.elapsed().as_secs_f64();
        if tok > 0 {
            steady += el;
        }
        let touched = (layers * active * expert_bytes) as f64;
        let extra = region
            .as_ref()
            .map(|r| {
                let s = r.stats();
                let gb = (s.bytes_read - read0) as f64 / 1e9;
                read0 = s.bytes_read;
                format!(
                    "  read={gb:.2} GB hard={} soft={}",
                    s.hard_faults, s.soft_faults
                )
            })
            .unwrap_or_default();
        println!(
            "token {tok}: {el:.2} s ({:.2} GB/s of weights){extra}  out={tok_sum:.6}",
            touched / el / 1e9
        );
    }
    let el = t_all.elapsed().as_secs_f64();
    let d1 = disk_stats(&dev);
    let steady_tps = if tokens > 1 {
        (tokens - 1) as f64 / steady
    } else {
        tokens as f64 / el
    };
    let extra = region
        .as_ref()
        .map(|r| {
            let s = r.stats();
            format!(
                " ddram_read_gb={:.2} hard_faults={} plan_hits={} plan_misses={} cyclic={}",
                s.bytes_read as f64 / 1e9,
                s.hard_faults,
                s.plan_hits,
                s.plan_misses,
                s.cyclic
            )
        })
        .unwrap_or_default();
    println!(
        "RESULT llm engine={engine} mode={mode} experts={experts} active={active} tokens_per_s={:.3} steady_tokens_per_s={steady_tps:.3} secs={el:.2} disk_read_gb={:.2} checksum={checksum:.6}{extra}",
        tokens as f64 / el,
        (d1.read - d0.read) as f64 / 1e9
    );
    #[cfg(unix)]
    if let Some((p, len)) = mapping {
        unsafe { libc::munmap(p, len) };
    }
    drop(region);
}
