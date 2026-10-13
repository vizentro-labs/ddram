// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! `libddram_shim.so`: run an unmodified program (llama.cpp, any loader that
//! `mmap`s its data read-only) with its large file mappings served by ddram.
//!
//! ```text
//! DDRAM_BUDGET=2G LD_PRELOAD=target/release/libddram_shim.so llama-completion -m model.gguf ...
//! ```
//!
//! The shim interposes `mmap`/`mmap64`, `munmap`, `madvise` and
//! `posix_madvise`. A read-only, offset-0 mapping of a file at least
//! `DDRAM_MIN_SIZE` bytes long becomes a [`ddram::Region`] over that file;
//! the program gets the region's pointer and never knows. Everything else is
//! passed through. While the program runs, the shim records the order in
//! which blocks are faulted in, and once every block has been seen once it
//! installs that order as a cyclic access [`ddram::Plan`] (a decode loop
//! reads the weights in the same order for every token) and saves it next to
//! the file (`<file>.ddram-plan`), so the next run starts with the plan from
//! its first token.
//!
//! Environment:
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `DDRAM_BUDGET` | (required) | RAM for the mapped data, e.g. `2G`. Without it the shim is inactive. |
//! | `DDRAM_MIN_SIZE` | `64M` | Smaller mappings are left to the kernel. |
//! | `DDRAM_BLOCK` | `1M` | Block size. |
//! | `DDRAM_MATCH` | (none) | Only files whose path contains this substring. |
//! | `DDRAM_PLAN` | `1` | `0` disables learning; `1` learns and installs; `2` also disables the saved-plan file. |
//! | `DDRAM_PLAN_FILE` | `<file>.ddram-plan` | Where the learned plan is stored (`-` to disable). |
//! | `DDRAM_VERBOSE` | `0` | `1` logs decisions to stderr. |
//!
//! Linux only (`LD_PRELOAD`); on other systems this crate is empty.

#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::ffi::{c_int, c_void, CStr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::*};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ddram::{Config, Plan, Policy, Region};

type MmapFn = unsafe extern "C" fn(*mut c_void, usize, c_int, c_int, c_int, libc::off_t) -> *mut c_void;
type MunmapFn = unsafe extern "C" fn(*mut c_void, usize) -> c_int;
type MadviseFn = unsafe extern "C" fn(*mut c_void, usize, c_int) -> c_int;

thread_local! {
    /// Set while the shim itself (or ddram on its behalf) is running, so
    /// nested calls go straight to the real functions.
    static INSIDE: Cell<bool> = const { Cell::new(false) };
}

struct Real {
    mmap: MmapFn,
    munmap: MunmapFn,
    madvise: MadviseFn,
    posix_madvise: MadviseFn,
}

fn real() -> &'static Real {
    static REAL: OnceLock<Real> = OnceLock::new();
    REAL.get_or_init(|| unsafe {
        let sym = |name: &[u8]| {
            let p = libc::dlsym(libc::RTLD_NEXT, name.as_ptr().cast());
            assert!(!p.is_null(), "ddram-shim: dlsym failed");
            p
        };
        Real {
            mmap: std::mem::transmute::<*mut c_void, MmapFn>(sym(b"mmap\0")),
            munmap: std::mem::transmute::<*mut c_void, MunmapFn>(sym(b"munmap\0")),
            madvise: std::mem::transmute::<*mut c_void, MadviseFn>(sym(b"madvise\0")),
            posix_madvise: std::mem::transmute::<*mut c_void, MadviseFn>(sym(b"posix_madvise\0")),
        }
    })
}

struct Settings {
    budget: usize,
    min_size: u64,
    block: usize,
    matcher: Option<String>,
    plan: u32,
    plan_file: Option<String>,
    verbose: bool,
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'G' => (&s[..s.len() - 1], 1 << 30),
        'T' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    Some((num.parse::<f64>().ok()? * mul as f64) as u64)
}

fn settings() -> Option<&'static Settings> {
    static S: OnceLock<Option<Settings>> = OnceLock::new();
    S.get_or_init(|| {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let budget = env("DDRAM_BUDGET").and_then(|v| parse_size(&v))? as usize;
        Some(Settings {
            budget,
            min_size: env("DDRAM_MIN_SIZE").and_then(|v| parse_size(&v)).unwrap_or(64 << 20),
            block: env("DDRAM_BLOCK").and_then(|v| parse_size(&v)).unwrap_or(1 << 20) as usize,
            matcher: env("DDRAM_MATCH"),
            plan: env("DDRAM_PLAN").and_then(|v| v.parse().ok()).unwrap_or(1),
            plan_file: env("DDRAM_PLAN_FILE"),
            verbose: env("DDRAM_VERBOSE").map(|v| v != "0").unwrap_or(false),
        })
    })
    .as_ref()
}

macro_rules! log {
    ($s:expr, $($arg:tt)*) => {
        if $s.verbose {
            eprintln!("ddram-shim: {}", format!($($arg)*));
        }
    };
}

struct Mapping {
    base: usize,
    len: usize,
    region: Region,
    plan_file: Option<PathBuf>,
    planned: bool,
}

/// Mappings served by ddram. Locked only in the interposed calls and the
/// monitor thread, never in the fault path.
fn mappings() -> &'static Mutex<Vec<Mapping>> {
    static M: OnceLock<Mutex<Vec<Mapping>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(Vec::new()))
}

static MONITOR: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

fn fd_path(fd: c_int) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()
}

/// Decide whether to serve this mapping, and build the region if so.
unsafe fn try_serve(len: usize, prot: c_int, flags: c_int, fd: c_int, off: libc::off_t) -> Option<*mut c_void> {
    let s = settings()?;
    if fd < 0
        || off != 0
        || prot & libc::PROT_WRITE != 0
        || flags & (libc::MAP_FIXED | libc::MAP_ANONYMOUS) != 0
        || (len as u64) < s.min_size
    {
        return None;
    }
    let path = fd_path(fd)?;
    let ps = path.to_string_lossy();
    if ps.starts_with("/dev/") || ps.starts_with("/proc/") || ps.starts_with("/sys/") {
        return None;
    }
    if let Some(m) = &s.matcher {
        if !ps.contains(m.as_str()) {
            return None;
        }
    }
    let size = std::fs::metadata(&path).ok()?.len();
    if size < len as u64 {
        return None;
    }
    let mut c = Config::read_only_file(&path, s.budget);
    c.block_size = s.block;
    c.policy = Policy::Auto;
    let region = match Region::new(c) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ddram-shim: cannot serve {ps}: {e}; using the kernel mapping");
            return None;
        }
    };
    let plan_file = match (s.plan, s.plan_file.as_deref()) {
        (0, _) | (2, _) | (_, Some("-")) => None,
        (_, Some(f)) => Some(PathBuf::from(f)),
        (_, None) => Some(PathBuf::from(format!("{ps}.ddram-plan"))),
    };
    let mut planned = false;
    if let Some(f) = &plan_file {
        if let Ok(p) = Plan::load(f) {
            if p.total_bytes() <= region.len() as u64 && region.set_plan(&p).is_ok() {
                planned = true;
                log!(s, "{ps}: loaded plan from {} ({} ranges)", f.display(), p.len());
            }
        }
    }
    if s.plan != 0 && !planned {
        let blocks = region.len() / region.block_size();
        region.record(2 * blocks + 1024);
        log!(s, "{ps}: recording the first pass ({blocks} blocks)");
    }
    let base = region.as_ptr() as usize;
    log!(
        s,
        "{ps}: serving {:.2} GB from a {:.2} GB RAM budget, {} KiB blocks",
        len as f64 / 1e9,
        s.budget as f64 / 1e9,
        s.block >> 10
    );
    mappings().lock().unwrap().push(Mapping {
        base,
        len,
        region,
        plan_file,
        planned,
    });
    ACTIVE.fetch_add(1, Relaxed);
    if s.plan != 0 && !MONITOR.swap(true, Relaxed) {
        let _ = std::thread::Builder::new()
            .name("ddram-shim-monitor".into())
            .spawn(monitor);
    }
    Some(base as *mut c_void)
}

/// Turn a completed first pass into a plan, install it and save it.
fn monitor() {
    INSIDE.with(|i| i.set(true));
    let s = settings().unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if ACTIVE.load(Relaxed) == 0 {
            continue;
        }
        let mut ms = mappings().lock().unwrap();
        for m in ms.iter_mut().filter(|m| !m.planned) {
            let Some((trace, full)) = m.region.recorded() else {
                continue;
            };
            let blocks = m.region.len() / m.region.block_size();
            // The first pass is complete when (nearly) every block has been
            // seen; a model's blocks are all read once per token. Fall back
            // to whatever was captured if the buffer fills up first.
            let mut seen = vec![false; blocks];
            let mut distinct = 0;
            for &b in &trace {
                if !seen[b as usize] {
                    seen[b as usize] = true;
                    distinct += 1;
                }
            }
            let complete = distinct >= blocks - blocks / 64;
            if !complete && !full {
                continue;
            }
            let plan = Plan::from_trace(&trace, m.region.block_size() as u64, true);
            m.region.stop_recording();
            if plan.is_empty() || m.region.set_plan(&plan).is_err() {
                continue;
            }
            m.planned = true;
            log!(
                s,
                "learned a plan of {} ranges covering {:.2} GB after {} faults",
                plan.len(),
                plan.total_bytes() as f64 / 1e9,
                trace.len()
            );
            if let Some(f) = &m.plan_file {
                match plan.save(f) {
                    Ok(()) => log!(s, "saved the plan to {}", f.display()),
                    Err(e) => log!(s, "cannot save the plan to {}: {e}", f.display()),
                }
            }
        }
    }
}

unsafe fn do_mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: libc::off_t) -> *mut c_void {
    if addr.is_null() && !INSIDE.with(|i| i.get()) {
        INSIDE.with(|i| i.set(true));
        let served = try_serve(len, prot, flags, fd, off);
        INSIDE.with(|i| i.set(false));
        if let Some(p) = served {
            return p;
        }
    }
    (real().mmap)(addr, len, prot, flags, fd, off)
}

/// # Safety
/// Same contract as `mmap(2)`.
#[no_mangle]
pub unsafe extern "C" fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: libc::off_t) -> *mut c_void {
    do_mmap(addr, len, prot, flags, fd, off)
}

/// # Safety
/// Same contract as `mmap64(3)`.
#[no_mangle]
pub unsafe extern "C" fn mmap64(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: libc::off_t) -> *mut c_void {
    do_mmap(addr, len, prot, flags, fd, off)
}

/// # Safety
/// Same contract as `munmap(2)`. Unmapping a served mapping as a whole
/// releases the region; unmapping part of one is ignored (the program is
/// done with those bytes, which costs nothing to keep).
#[no_mangle]
pub unsafe extern "C" fn munmap(addr: *mut c_void, len: usize) -> c_int {
    if !INSIDE.with(|i| i.get()) && ACTIVE.load(Relaxed) > 0 {
        let a = addr as usize;
        let mut hit = false;
        let mut whole = None;
        {
            let mut ms = mappings().lock().unwrap();
            if let Some(i) = ms.iter().position(|m| a >= m.base && a < m.base + m.len) {
                hit = true;
                if a == ms[i].base && len >= ms[i].len {
                    whole = Some(ms.remove(i));
                }
            }
        }
        if let Some(m) = whole {
            INSIDE.with(|i| i.set(true));
            drop(m);
            INSIDE.with(|i| i.set(false));
            ACTIVE.fetch_sub(1, Relaxed);
        }
        if hit {
            return 0;
        }
    }
    (real().munmap)(addr, len)
}

unsafe fn served(addr: *mut c_void) -> bool {
    if INSIDE.with(|i| i.get()) || ACTIVE.load(Relaxed) == 0 {
        return false;
    }
    let a = addr as usize;
    mappings()
        .lock()
        .unwrap()
        .iter()
        .any(|m| a >= m.base && a < m.base + m.len)
}

/// # Safety
/// Same contract as `madvise(2)`. Advice on served mappings is accepted and
/// ignored (the plan knows better).
#[no_mangle]
pub unsafe extern "C" fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int {
    if served(addr) {
        return 0;
    }
    (real().madvise)(addr, len, advice)
}

/// # Safety
/// Same contract as `posix_madvise(3)`.
#[no_mangle]
pub unsafe extern "C" fn posix_madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int {
    if served(addr) {
        return 0;
    }
    (real().posix_madvise)(addr, len, advice)
}
