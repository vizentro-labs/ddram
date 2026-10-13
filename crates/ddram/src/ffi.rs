// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! C ABI. See `include/ddram.h`.

use std::ffi::{c_char, c_int, CStr};
use std::ptr;

use crate::{Advice, Config, Plan, Policy, Region};

/// Options for `ddram_region_create`; zero fields take defaults.
#[repr(C)]
pub struct DdramOptions {
    pub size: u64,
    pub ram_budget: u64,
    pub block_size: u64,
    pub prefetch_depth: u32,
    pub io_threads: u32,
    pub write_rate_limit: u64,
    /// 0 = auto, 1 = clock, 2 = cyclic
    pub policy: u32,
    pub read_only: u32,
    pub persist: u32,
    pub buffered_io: u32,
}

#[repr(C)]
pub struct DdramStats {
    pub hard_faults: u64,
    pub soft_faults: u64,
    pub write_faults: u64,
    pub loads: u64,
    pub prefetch_loads: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub evictions: u64,
    pub writebacks: u64,
    pub resident_blocks: u64,
    pub plan_hits: u64,
    pub plan_misses: u64,
    pub plan_position: u64,
}

/// Opaque handle (`ddram_region` in C).
pub struct DdramRegion(Region);

/// Opaque access plan (`ddram_plan` in C).
pub struct DdramPlan(Plan);

/// New empty plan; `cyclic != 0` means the sequence repeats.
#[no_mangle]
pub extern "C" fn ddram_plan_new(cyclic: c_int) -> *mut DdramPlan {
    let mut p = Plan::new();
    p.set_cyclic(cyclic != 0);
    Box::into_raw(Box::new(DdramPlan(p)))
}

/// # Safety
/// `p` must come from `ddram_plan_new`/`ddram_plan_load` and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn ddram_plan_free(p: *mut DdramPlan) {
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}

/// Append the byte range `[off, off+len)` as the next thing accessed.
///
/// # Safety
/// `p` must be a live plan.
#[no_mangle]
pub unsafe extern "C" fn ddram_plan_add(p: *mut DdramPlan, off: u64, len: u64) {
    (*p).0.push(off, len);
}

/// Bytes kept resident ahead of the plan position (0 = a quarter of the budget).
///
/// # Safety
/// `p` must be a live plan.
#[no_mangle]
pub unsafe extern "C" fn ddram_plan_set_lookahead(p: *mut DdramPlan, bytes: u64) {
    (*p).0.set_lookahead(bytes);
}

/// # Safety
/// `p` must be a live plan.
#[no_mangle]
pub unsafe extern "C" fn ddram_plan_len(p: *const DdramPlan) -> u64 {
    (*p).0.len() as u64
}

/// Write the plan to `path`. Returns 0 on success.
///
/// # Safety
/// `p` must be a live plan and `path` a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn ddram_plan_save(p: *const DdramPlan, path: *const c_char) -> c_int {
    let Ok(path) = CStr::from_ptr(path).to_str() else {
        return -1;
    };
    match (*p).0.save(path) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Read a plan written by `ddram_plan_save`. Returns NULL on error.
///
/// # Safety
/// `path` must be a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn ddram_plan_load(path: *const c_char) -> *mut DdramPlan {
    let Ok(path) = CStr::from_ptr(path).to_str() else {
        return ptr::null_mut();
    };
    match Plan::load(path) {
        Ok(p) => Box::into_raw(Box::new(DdramPlan(p))),
        Err(_) => ptr::null_mut(),
    }
}

/// Install a copy of `p` as the region's access plan. Returns 0 on success.
///
/// # Safety
/// `r` must be a live region and `p` a live plan.
#[no_mangle]
pub unsafe extern "C" fn ddram_set_plan(r: *const DdramRegion, p: *const DdramPlan) -> c_int {
    match (*r).0.set_plan(&(*p).0) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ddram_set_plan: {e}");
            -1
        }
    }
}

/// Remove the access plan.
///
/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_clear_plan(r: *const DdramRegion) {
    (*r).0.clear_plan();
}

/// Start recording the fault order (up to `max_entries` blocks).
///
/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_record(r: *const DdramRegion, max_entries: u64) {
    (*r).0.record(max_entries as usize);
}

/// Stop recording.
///
/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_stop_recording(r: *const DdramRegion) {
    (*r).0.stop_recording();
}

/// The recorded access order as a plan (one pass), or NULL if nothing was
/// recorded. Free with `ddram_plan_free`.
///
/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_recorded_plan(r: *const DdramRegion, cyclic: c_int) -> *mut DdramPlan {
    match (*r).0.recorded_plan(cyclic != 0) {
        Some(p) => Box::into_raw(Box::new(DdramPlan(p))),
        None => ptr::null_mut(),
    }
}

/// Create a region backed by `path` (file, or directory for a temp file).
/// Returns NULL on error.
///
/// # Safety
/// `path` must be a valid NUL-terminated string and `opts` a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn ddram_region_create(
    path: *const c_char,
    opts: *const DdramOptions,
) -> *mut DdramRegion {
    if path.is_null() || opts.is_null() {
        return ptr::null_mut();
    }
    let o = &*opts;
    let p = match CStr::from_ptr(path).to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let mut c = Config::new(o.size, o.ram_budget as usize, p);
    if o.block_size != 0 {
        c.block_size = o.block_size as usize;
    }
    if o.prefetch_depth != 0 {
        c.prefetch_depth = o.prefetch_depth as usize;
    }
    if o.io_threads != 0 {
        c.io_threads = o.io_threads as usize;
    }
    c.write_rate_limit = o.write_rate_limit;
    c.policy = match o.policy {
        1 => Policy::Clock,
        2 => Policy::Cyclic,
        _ => Policy::Auto,
    };
    c.read_only = o.read_only != 0;
    c.persist = o.persist != 0;
    c.direct_io = o.buffered_io == 0;
    match Region::new(c) {
        Ok(r) => Box::into_raw(Box::new(DdramRegion(r))),
        Err(e) => {
            eprintln!("ddram_region_create: {e}");
            ptr::null_mut()
        }
    }
}

/// # Safety
/// `r` must come from `ddram_region_create` and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn ddram_region_destroy(r: *mut DdramRegion) {
    if !r.is_null() {
        drop(Box::from_raw(r));
    }
}

/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_region_ptr(r: *const DdramRegion) -> *mut u8 {
    (*r).0.as_ptr()
}

/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_region_len(r: *const DdramRegion) -> u64 {
    (*r).0.len() as u64
}

/// # Safety
/// `r` must be a live region; out pointers may be NULL.
#[no_mangle]
pub unsafe extern "C" fn ddram_region_arena(
    r: *const DdramRegion,
    ptr_out: *mut *mut u8,
    len_out: *mut u64,
) {
    let (p, l) = (*r).0.arena();
    if !ptr_out.is_null() {
        *ptr_out = p;
    }
    if !len_out.is_null() {
        *len_out = l as u64;
    }
}

/// 0 normal, 1 random, 2 sequential, 3 cyclic, 4 willneed, 5 cold, 6 hot.
///
/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_advise(
    r: *const DdramRegion,
    off: u64,
    len: u64,
    advice: c_int,
) -> c_int {
    let a = match advice {
        0 => Advice::Normal,
        1 => Advice::Random,
        2 => Advice::Sequential,
        3 => Advice::Cyclic,
        4 => Advice::WillNeed,
        5 => Advice::Cold,
        6 => Advice::Hot,
        _ => return -1,
    };
    (*r).0.advise(off as usize, len as usize, a);
    0
}

/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_prefetch(r: *const DdramRegion, off: u64, len: u64) {
    (*r).0.prefetch(off as usize, len as usize);
}

/// Pin block `index`; returns a pointer to its bytes in the arena and writes
/// the arena offset (for GPU uploads) to `arena_off` if non-NULL. Must be
/// balanced by `ddram_unpin(r, index)`.
///
/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_pin(
    r: *const DdramRegion,
    index: u64,
    writable: c_int,
    arena_off: *mut u64,
) -> *mut u8 {
    let reg = &(*r).0;
    let (ptr, off) = if writable != 0 {
        let p = reg.pin_mut(index as usize);
        let v = (p.as_ptr() as *mut u8, p.arena_offset());
        std::mem::forget(p);
        v
    } else {
        let p = reg.pin(index as usize);
        let v = (p.as_ptr() as *mut u8, p.arena_offset());
        std::mem::forget(p);
        v
    };
    if !arena_off.is_null() {
        *arena_off = off as u64;
    }
    ptr
}

/// # Safety
/// `r` must be a live region and `index` currently pinned by the caller.
#[no_mangle]
pub unsafe extern "C" fn ddram_unpin(r: *const DdramRegion, index: u64) {
    (*r).0.unpin_block(index as usize);
}

/// # Safety
/// `r` must be a live region.
#[no_mangle]
pub unsafe extern "C" fn ddram_flush(r: *const DdramRegion) -> c_int {
    match (*r).0.flush() {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// # Safety
/// `r` must be a live region and `out` valid.
#[no_mangle]
pub unsafe extern "C" fn ddram_stats_get(r: *const DdramRegion, out: *mut DdramStats) {
    let s = (*r).0.stats();
    *out = DdramStats {
        hard_faults: s.hard_faults,
        soft_faults: s.soft_faults,
        write_faults: s.write_faults,
        loads: s.loads,
        prefetch_loads: s.prefetch_loads,
        bytes_read: s.bytes_read,
        bytes_written: s.bytes_written,
        evictions: s.evictions,
        writebacks: s.writebacks,
        resident_blocks: s.resident_blocks,
        plan_hits: s.plan_hits,
        plan_misses: s.plan_misses,
        plan_position: s.plan_position,
    };
}
