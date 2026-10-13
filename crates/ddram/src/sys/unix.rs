// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Unix (Linux, macOS) virtual-memory and fault-handling primitives.
//!
//! The frame arena is a shared-memory object (memfd on Linux, an unlinked
//! POSIX shm object on macOS) mapped once read/write as the "backdoor" view.
//! Blocks of the application-visible region are made resident by mapping the
//! frame that holds them over the block's address with `MAP_FIXED`, and made
//! non-resident again by mapping anonymous `PROT_NONE` memory over it.

use std::io;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

pub type RawHandle = libc::c_int;

pub fn page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// Smallest block size the mapping primitives support.
pub fn map_granularity() -> usize {
    page_size()
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

#[cfg(target_os = "linux")]
fn shm_fd() -> io::Result<RawHandle> {
    let name = b"ddram-arena\0";
    let fd = unsafe { libc::memfd_create(name.as_ptr().cast(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(last_err());
    }
    Ok(fd)
}

#[cfg(not(target_os = "linux"))]
fn shm_fd() -> io::Result<RawHandle> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    // POSIX shm names on macOS are limited to 31 bytes.
    let name = format!(
        "/ddram.{}.{}\0",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let fd = unsafe {
        libc::shm_open(
            name.as_ptr().cast(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(last_err());
    }
    unsafe { libc::shm_unlink(name.as_ptr().cast()) };
    Ok(fd)
}

pub fn create_arena(len: usize) -> io::Result<Arena> {
    let fd = shm_fd()?;
    if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
        let e = last_err();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        let e = last_err();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(Arena {
        ptr: p.cast(),
        len,
        fd,
    })
}

pub fn destroy_arena(a: &Arena) {
    unsafe {
        libc::munmap(a.ptr.cast(), a.len);
        libc::close(a.fd);
    }
}

#[cfg(target_os = "linux")]
const NORESERVE: libc::c_int = libc::MAP_NORESERVE;
#[cfg(not(target_os = "linux"))]
const NORESERVE: libc::c_int = 0;

/// Reserve `len` bytes of address space, aligned to `align`, inaccessible.
pub fn reserve(len: usize, align: usize, _block: usize) -> io::Result<*mut u8> {
    let total = len + align;
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            total,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON | NORESERVE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(last_err());
    }
    let start = p as usize;
    let aligned = (start + align - 1) & !(align - 1);
    unsafe {
        if aligned > start {
            libc::munmap(p, aligned - start);
        }
        let end = aligned + len;
        let tail = start + total - end;
        if tail > 0 {
            libc::munmap(end as *mut libc::c_void, tail);
        }
    }
    Ok(aligned as *mut u8)
}

pub unsafe fn release(base: *mut u8, len: usize, _block: usize) {
    libc::munmap(base.cast(), len);
}

/// Map arena frame bytes `[off, off+len)` at `addr`.
pub unsafe fn map_frame(
    a: &Arena,
    off: usize,
    addr: *mut u8,
    len: usize,
    writable: bool,
) -> io::Result<()> {
    let prot = if writable {
        libc::PROT_READ | libc::PROT_WRITE
    } else {
        libc::PROT_READ
    };
    let p = libc::mmap(
        addr.cast(),
        len,
        prot,
        libc::MAP_SHARED | libc::MAP_FIXED,
        a.fd,
        off as libc::off_t,
    );
    if p == libc::MAP_FAILED {
        return Err(last_err());
    }
    Ok(())
}

/// Replace whatever is mapped at `addr` with inaccessible memory.
pub unsafe fn unmap_block(addr: *mut u8, len: usize) -> io::Result<()> {
    let p = libc::mmap(
        addr.cast(),
        len,
        libc::PROT_NONE,
        libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED | NORESERVE,
        -1,
        0,
    );
    if p == libc::MAP_FAILED {
        return Err(last_err());
    }
    Ok(())
}

pub unsafe fn protect_rw(addr: *mut u8, len: usize) -> io::Result<()> {
    if libc::mprotect(addr.cast(), len, libc::PROT_READ | libc::PROT_WRITE) != 0 {
        return Err(last_err());
    }
    Ok(())
}

pub unsafe fn protect_ro(addr: *mut u8, len: usize) -> io::Result<()> {
    if libc::mprotect(addr.cast(), len, libc::PROT_READ) != 0 {
        return Err(last_err());
    }
    Ok(())
}

pub fn sleep_us(us: u64) {
    let ts = libc::timespec {
        tv_sec: (us / 1_000_000) as libc::time_t,
        tv_nsec: ((us % 1_000_000) * 1000) as libc::c_long,
    };
    unsafe { libc::nanosleep(&ts, ptr::null_mut()) };
}

pub fn yield_now() {
    unsafe { libc::sched_yield() };
}

/// Write a message to stderr without allocating (usable from a fault handler).
pub fn raw_stderr(msg: &[u8]) {
    unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
}

// ---------------------------------------------------------------------------
// Fault handling
// ---------------------------------------------------------------------------

pub type FaultCallback = fn(addr: usize, write: Option<bool>) -> bool;

static CALLBACK: AtomicUsize = AtomicUsize::new(0);
static mut PREV_SEGV: std::mem::MaybeUninit<libc::sigaction> = std::mem::MaybeUninit::uninit();
static mut PREV_BUS: std::mem::MaybeUninit<libc::sigaction> = std::mem::MaybeUninit::uninit();

/// Best-effort decode of whether the faulting access was a write.
#[allow(unused_variables)]
unsafe fn fault_is_write(ctx: *mut libc::c_void) -> Option<bool> {
    if ctx.is_null() {
        return None;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let uc = &*(ctx as *const libc::ucontext_t);
        let err = uc.uc_mcontext.gregs[libc::REG_ERR as usize];
        return Some(err & 0x2 != 0);
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let uc = &*(ctx as *const libc::ucontext_t);
        let mc = uc.uc_mcontext;
        if mc.is_null() {
            return None;
        }
        // ESR_EL1.ISS.WnR (bit 6) is set for writes on data aborts.
        let esr = (*mc).__es.__esr;
        return Some(esr & (1 << 6) != 0);
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        let uc = &*(ctx as *const libc::ucontext_t);
        let mc = uc.uc_mcontext;
        if mc.is_null() {
            return None;
        }
        return Some((*mc).__es.__err & 0x2 != 0);
    }
    #[allow(unreachable_code)]
    None
}

unsafe fn chain(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    let prev = if sig == libc::SIGBUS {
        (*ptr::addr_of!(PREV_BUS)).assume_init_ref()
    } else {
        (*ptr::addr_of!(PREV_SEGV)).assume_init_ref()
    };
    let h = prev.sa_sigaction;
    if h == libc::SIG_DFL || h == libc::SIG_IGN {
        // Restore the default disposition; returning re-executes the faulting
        // instruction, which now terminates the process with the right signal.
        let mut dfl: libc::sigaction = std::mem::zeroed();
        dfl.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(sig, &dfl, ptr::null_mut());
    } else if prev.sa_flags & libc::SA_SIGINFO != 0 {
        let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
            std::mem::transmute(h);
        f(sig, info, ctx);
    } else {
        let f: extern "C" fn(libc::c_int) = std::mem::transmute(h);
        f(sig);
    }
}

extern "C" fn on_signal(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    unsafe {
        let cb = CALLBACK.load(Ordering::Acquire);
        if cb != 0 && !info.is_null() {
            #[cfg(target_os = "linux")]
            let addr = (*info).si_addr() as usize;
            #[cfg(not(target_os = "linux"))]
            let addr = (*info).si_addr as usize;
            let f: FaultCallback = std::mem::transmute(cb);
            let saved = *errno_loc();
            let handled = f(addr, fault_is_write(ctx));
            *errno_loc() = saved;
            if handled {
                return;
            }
        }
        chain(sig, info, ctx);
    }
}

#[cfg(target_os = "linux")]
unsafe fn errno_loc() -> *mut libc::c_int {
    libc::__errno_location()
}
#[cfg(not(target_os = "linux"))]
unsafe fn errno_loc() -> *mut libc::c_int {
    libc::__error()
}

pub fn install_fault_handler(cb: FaultCallback) -> io::Result<()> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    CALLBACK.store(cb as *const () as usize, Ordering::Release);
    let mut res = Ok(());
    ONCE.call_once(|| unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(
            libc::SIGSEGV,
            &sa,
            (*ptr::addr_of_mut!(PREV_SEGV)).as_mut_ptr(),
        ) != 0
            || libc::sigaction(
                libc::SIGBUS,
                &sa,
                (*ptr::addr_of_mut!(PREV_BUS)).as_mut_ptr(),
            ) != 0
        {
            res = Err(last_err());
        }
    });
    res
}
