// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! `ddram` command line.
//!
//! ```text
//! ddram smart  [PATH]                       NVMe health of the drive holding PATH
//! ddram budget [PATH] [--rated-tbw TB] [--years N] [--share F]
//! ddram bench  --engine ddram|mmap|swap --workload W [options]
//! ```

mod bench;
mod llm;

use std::path::PathBuf;

use ddram::smart::{self, EnduranceBudget};

pub fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let (num, mul) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        Some('T') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    (num.parse::<f64>()
        .unwrap_or_else(|_| panic!("bad size: {s}"))
        * mul as f64) as u64
}

pub struct Args {
    pub pos: Vec<String>,
    pub opts: Vec<(String, String)>,
}

impl Args {
    pub fn parse(it: impl Iterator<Item = String>) -> Args {
        let mut pos = Vec::new();
        let mut opts = Vec::new();
        let v: Vec<String> = it.collect();
        let mut i = 0;
        while i < v.len() {
            if let Some(k) = v[i].strip_prefix("--") {
                if let Some((k, val)) = k.split_once('=') {
                    opts.push((k.to_string(), val.to_string()));
                } else if i + 1 < v.len() && !v[i + 1].starts_with("--") {
                    opts.push((k.to_string(), v[i + 1].clone()));
                    i += 1;
                } else {
                    opts.push((k.to_string(), "true".into()));
                }
            } else {
                pos.push(v[i].clone());
            }
            i += 1;
        }
        Args { pos, opts }
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.opts
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
    pub fn get_or<'a>(&'a self, k: &str, d: &'a str) -> &'a str {
        self.get(k).unwrap_or(d)
    }
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  ddram smart  [PATH|DEVICE]\n  ddram budget [PATH|DEVICE] [--rated-tbw TB] [--years 5] [--share 0.25]\n  \
         ddram bench  --engine ddram|mmap|swap --workload fill|cyclic|chunks|random|zipf|zipf-write\n               \
         [--size 8G] [--budget 2G] [--dir .] [--block 1M] [--passes 3] [--ops 200000] [--threads 1] [--cgroup]\n               \
         [--plan] [--chunk 4M] [--policy auto|clock|cyclic]\n  \
         ddram llm    --engine ddram|mmap [--model-size 8G] [--budget 2G] [--layers 32] [--tokens 4] [--dir .] [--cgroup]\n               \
         [--plan] [--experts 1] [--active 1] [--expert-skew 1.0] [--threads 4] [--block 1M]"
    );
    std::process::exit(2)
}

fn device_arg(a: &Args) -> smart::Device {
    let p = a.pos.get(1).cloned().unwrap_or_else(|| ".".into());
    let looks_like_dev =
        p.starts_with("/dev/nvme") || p.starts_with(r"\\.\") || p.starts_with("disk");
    if looks_like_dev {
        return smart::Device { path: p };
    }
    match smart::device_for_path(&PathBuf::from(&p)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot find NVMe device for {p}: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_smart(a: &Args) {
    let dev = device_arg(a);
    match smart::read_health(&dev) {
        Ok(h) => {
            println!("device                    : {}", dev.path);
            println!("{h}");
            let w = h.warnings();
            if w.is_empty() {
                println!("status                    : OK for tiering");
            } else {
                for x in w {
                    println!("WARNING                   : {x}");
                }
                if h.is_unsafe_for_tiering() {
                    println!("status                    : UNSAFE for tiering");
                }
            }
        }
        Err(e) => {
            eprintln!("reading health log from {}: {e}", dev.path);
            std::process::exit(1);
        }
    }
}

fn cmd_budget(a: &Args) {
    let dev = device_arg(a);
    let h = match smart::read_health(&dev) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("reading health log from {}: {e}", dev.path);
            std::process::exit(1);
        }
    };
    let rated = a.get("rated-tbw").map(|v| v.parse::<f64>().unwrap() * 1e12);
    let years: f64 = a.get_or("years", "5").parse().unwrap();
    let share: f64 = a.get_or("share", "0.25").parse().unwrap();
    let b = EnduranceBudget::from_health(&h, rated, 300e12, years);
    println!("device            : {}", dev.path);
    println!(
        "endurance (TBW)   : {:.1} TB ({})",
        b.total_bytes / 1e12,
        b.source
    );
    println!("written           : {:.2} TB", b.written_bytes / 1e12);
    println!(
        "remaining         : {:.1} TB over {:.0} days",
        b.remaining_bytes / 1e12,
        b.remaining_days
    );
    println!("daily budget      : {:.1} GB/day", b.daily_bytes / 1e9);
    println!(
        "governor ({:>3.0}%)   : {:.2} MB/s sustained write-back",
        share * 100.0,
        b.rate_for_share(share) as f64 / 1e6
    );
}

fn main() {
    let a = Args::parse(std::env::args().skip(1));
    match a.pos.first().map(|s| s.as_str()) {
        Some("smart") => cmd_smart(&a),
        Some("budget") => cmd_budget(&a),
        Some("bench") => bench::run(&a),
        Some("llm") => llm::run(&a),
        _ => usage(),
    }
}
