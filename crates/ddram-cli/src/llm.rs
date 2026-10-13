// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! `ddram llm`: a synthetic LLM decode loop that streams weights from a file.
//!
//! Each "token" runs a matrix-vector product through every layer, which means
//! reading every weight once per token, as batch-1 transformer decoding does.
//! The model file is mapped read-only either through ddram (with the cyclic
//! policy and per-layer prefetch) or through the kernel (`mmap` +
//! `MADV_SEQUENTIAL`/`MADV_WILLNEED`, i.e. what llama.cpp does by default), under
//! the same memory limit. Outputs carry a checksum so both engines are proven
//! to compute identical results.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ddram::{Advice, Config, Policy, Region};

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

/// y = W x for a [rows x D] row-major block, split across threads.
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
                    *out = acc.iter().sum();
                }
            });
        }
    });
}

pub fn run(a: &Args) {
    let size = parse_size(a.get_or("model-size", "8G"));
    let budget = parse_size(a.get_or("budget", "2G")) as usize;
    let layers: usize = a.get_or("layers", "32").parse().unwrap();
    let tokens: usize = a.get_or("tokens", "4").parse().unwrap();
    let threads: usize = a.get_or("threads", "4").parse().unwrap();
    let dir = PathBuf::from(a.get_or("dir", "."));
    let engine = a.get_or("engine", "ddram").to_string();
    let block = parse_size(a.get_or("block", "1M")) as usize;

    let layer_rows = (size as usize / 4 / layers / D) & !7;
    let layer_floats = layer_rows * D;
    let model_bytes = (layer_floats * layers * 4) as u64;
    let path = dir.join(format!("ddram-llm-{model_bytes}.weights"));
    gen_model(&path, model_bytes);
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");

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
        c.policy = Policy::Cyclic;
        c.prefetch_depth = 32;
        Region::new(c).expect("region")
    });
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

    println!(
        "# llm engine={engine} model={:.2} GB layers={layers} rows/layer={layer_rows} hidden={D} budget={:.2} GB threads={threads}",
        model_bytes as f64 / 1e9,
        budget as f64 / 1e9
    );

    let mut x = vec![1.0f32 / D as f32; D];
    let mut y = vec![0f32; layer_rows];
    let t_all = Instant::now();
    let mut checksum = 0f64;
    for tok in 0..tokens {
        let t = Instant::now();
        for l in 0..layers {
            let off = l * layer_floats;
            // Hint the next layer so I/O overlaps this layer's compute.
            let next = ((l + 1) % layers) * layer_floats * 4;
            match (&region, engine.as_str()) {
                (Some(r), _) => r.advise(next, layer_floats * 4, Advice::WillNeed),
                #[cfg(unix)]
                (None, "mmap") => unsafe {
                    let (p, _) = mapping.unwrap();
                    libc::madvise(
                        (p as *mut u8).add(next).cast(),
                        layer_floats * 4,
                        libc::MADV_WILLNEED,
                    );
                },
                _ => {}
            }
            let w = unsafe { std::slice::from_raw_parts(base.add(off), layer_floats) };
            matvec(w, &x, &mut y, threads);
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
        println!(
            "token {tok}: {:.2} s ({:.2} GB/s of weights)  out={tok_sum:.6}",
            t.elapsed().as_secs_f64(),
            model_bytes as f64 / t.elapsed().as_secs_f64() / 1e9
        );
    }
    let el = t_all.elapsed().as_secs_f64();
    let extra = region
        .as_ref()
        .map(|r| {
            let s = r.stats();
            format!(
                " ddram_read_gb={:.2} cyclic={}",
                s.bytes_read as f64 / 1e9,
                s.cyclic
            )
        })
        .unwrap_or_default();
    println!(
        "RESULT llm engine={engine} tokens_per_s={:.3} secs={el:.2} checksum={checksum:.6}{extra}",
        tokens as f64 / el
    );
    #[cfg(unix)]
    if let Some((p, len)) = mapping {
        unsafe { libc::munmap(p, len) };
    }
    drop(region);
}
