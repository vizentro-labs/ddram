// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Windows virtual-memory and fault-handling primitives.
//!
//! The frame arena is a pagefile-backed section mapped once as the backdoor
//! view. The application region is reserved as placeholders (one per block,
//! Windows 10 1803+), so a block is made resident with `MapViewOfFile3`
//! (`MEM_REPLACE_PLACEHOLDER`) of the frame's section offset, and evicted with
//! `UnmapViewOfFile2` (`MEM_PRESERVE_PLACEHOLDER`). Access to a placeholder
//! raises `EXCEPTION_ACCESS_VIOLATION`, handled by a vectored handler.

use std::io;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, EXCEPTION_ACCESS_VIOLATION, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, MapViewOfFile3, UnmapViewOfFile, UnmapViewOfFile2,
    VirtualAlloc2, VirtualFree, VirtualProtect, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, Sleep, SwitchToThread};

pub type RawHandle = HANDLE;

const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

fn sysinfo() -> SYSTEM_INFO {
    unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        si
    }
}

#[allow(dead_code)]
pub fn page_size() -> usize {
    sysinfo().dwPageSize as usize
}

/// Views and placeholders must be aligned to the allocation granularity (64 KiB).
pub fn map_granularity() -> usize {
    sysinfo().dwAllocationGranularity as usize
}

pub struct Arena {
    pub ptr: *mut u8,
    pub len: usize,
    pub fd: RawHandle,
}

unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

fn last_err() -> io::Error {
    io::Error::last_os_error()
}

pub fn create_arena(len: usize) -> io::Result<Arena> {
    unsafe {
        let h = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            ptr::null(),
            PAGE_READWRITE,
            (len as u64 >> 32) as u32,
            len as u32,
            ptr::null(),
        );
        if h.is_null() {
            return Err(last_err());
        }
        let v = MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, len);
        if v.Value.is_null() {
            let e = last_err();
            CloseHandle(h);
            return Err(e);
        }
        Ok(Arena {
            ptr: v.Value.cast(),
            len,
            fd: h,
        })
    }
}

pub fn destroy_arena(a: &Arena) {
    unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
            Value: a.ptr.cast(),
        });
        CloseHandle(a.fd);
    }
}

/// Reserve the region as one placeholder per block.
pub fn reserve(len: usize, _align: usize, block: usize) -> io::Result<*mut u8> {
    unsafe {
        let base = VirtualAlloc2(
            GetCurrentProcess(),
            ptr::null(),
            len,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            ptr::null_mut(),
            0,
        );
        if base.is_null() {
            return Err(last_err());
        }
        // Split into per-block placeholders, peeling one block off the front
        // of the remaining placeholder each time.
        let mut off = 0;
        while off + block < len {
            let p = (base as usize + off) as *mut core::ffi::c_void;
            if VirtualFree(p, block, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) == 0 {
                return Err(last_err());
            }
            off += block;
        }
        Ok(base.cast())
    }
}

pub unsafe fn release(base: *mut u8, len: usize, block: usize) {
    let mut off = 0;
    while off < len {
        VirtualFree(
            (base as usize + off) as *mut core::ffi::c_void,
            0,
            MEM_RELEASE,
        );
        off += block;
    }
}

pub unsafe fn map_frame(
    a: &Arena,
    off: usize,
    addr: *mut u8,
    len: usize,
    writable: bool,
) -> io::Result<()> {
    // A view's protection can later be lowered but never raised above what it
    // was mapped with, so always map read/write and then write-protect clean
    // blocks; the first write faults and `protect_rw` restores access.
    let v = MapViewOfFile3(
        a.fd,
        GetCurrentProcess(),
        addr.cast(),
        off as u64,
        len,
        MEM_REPLACE_PLACEHOLDER,
        PAGE_READWRITE,
        ptr::null_mut(),
        0,
    );
    if v.Value.is_null() {
        return Err(last_err());
    }
    if !writable {
        protect_ro(addr, len)?;
    }
    Ok(())
}

pub unsafe fn unmap_block(addr: *mut u8, _len: usize) -> io::Result<()> {
    if UnmapViewOfFile2(
        GetCurrentProcess(),
        MEMORY_MAPPED_VIEW_ADDRESS { Value: addr.cast() },
        MEM_PRESERVE_PLACEHOLDER,
    ) == 0
    {
        return Err(last_err());
    }
    Ok(())
}

pub unsafe fn protect_rw(addr: *mut u8, len: usize) -> io::Result<()> {
    let mut old = 0u32;
    if VirtualProtect(addr.cast(), len, PAGE_READWRITE, &mut old) == 0 {
        return Err(last_err());
    }
    Ok(())
}

pub unsafe fn protect_ro(addr: *mut u8, len: usize) -> io::Result<()> {
    let mut old = 0u32;
    if VirtualProtect(addr.cast(), len, PAGE_READONLY, &mut old) == 0 {
        return Err(last_err());
    }
    Ok(())
}

pub fn sleep_us(us: u64) {
    unsafe { Sleep(us.div_ceil(1000).max(1) as u32) };
}

pub fn yield_now() {
    unsafe { SwitchToThread() };
}

pub fn raw_stderr(msg: &[u8]) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(msg);
}

pub type FaultCallback = fn(addr: usize, write: Option<bool>) -> bool;

static CALLBACK: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn on_exception(info: *mut EXCEPTION_POINTERS) -> i32 {
    let rec = (*info).ExceptionRecord;
    if rec.is_null()
        || (*rec).ExceptionCode != EXCEPTION_ACCESS_VIOLATION
        || (*rec).NumberParameters < 2
    {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let cb = CALLBACK.load(Ordering::Acquire);
    if cb == 0 {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let kind = (*rec).ExceptionInformation[0];
    let addr = (*rec).ExceptionInformation[1];
    let write = match kind {
        0 => Some(false),
        1 => Some(true),
        _ => return EXCEPTION_CONTINUE_SEARCH, // DEP / execute fault
    };
    let f: FaultCallback = std::mem::transmute(cb);
    if f(addr, write) {
        EXCEPTION_CONTINUE_EXECUTION
    } else {
        EXCEPTION_CONTINUE_SEARCH
    }
}

pub fn install_fault_handler(cb: FaultCallback) -> io::Result<()> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    CALLBACK.store(cb as *const () as usize, Ordering::Release);
    let mut res = Ok(());
    ONCE.call_once(|| unsafe {
        if AddVectoredExceptionHandler(1, Some(on_exception)).is_null() {
            res = Err(last_err());
        }
    });
    res
}
