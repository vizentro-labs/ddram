# ddramnvme — platform research (Linux / macOS / Windows)

Status: research notes for implementation. Compiled 2026-10-13.

How it was researched: the sandbox could not reach man7.org, docs.kernel.org,
learn.microsoft.com, docs.nvidia.com, docs.rs or crates.io. Primary sources were
read from their upstream source repositories instead. These are the Linux tree
(kernel `Makefile` = v7.3-rc7 at time of reading), `MicrosoftDocs/sdk-api` and
`MicrosoftDocs/win32` (the Markdown sources of learn.microsoft.com), Apple's
DocC JSON (`developer.apple.com/tutorials/data/documentation/...`), the
`apple-oss-distributions/xnu` source, the Ubuntu `cufile.h(3)` man page, and
each crate's upstream `Cargo.toml`. Canonical URLs are cited next to the source
that was read.

Legend: **[verified]** = read in a primary source during this research.
**[recall]** = from prior knowledge and not re-checked here, so the code must
probe it at runtime. Kernel versions in particular must be probed with
`UFFDIO_API`/feature bits and never hard-coded.

---

## 1. Linux

### 1.1 userfaultfd (uffd): the transparent-mode fault mechanism

Sources: `include/uapi/linux/userfaultfd.h`
(https://raw.githubusercontent.com/torvalds/linux/master/include/uapi/linux/userfaultfd.h),
`Documentation/admin-guide/mm/userfaultfd.rst`
(https://docs.kernel.org/admin-guide/mm/userfaultfd.html, read via
https://raw.githubusercontent.com/torvalds/linux/master/Documentation/admin-guide/mm/userfaultfd.rst),
man pages https://man7.org/linux/man-pages/man2/userfaultfd.2.html,
https://man7.org/linux/man-pages/man2/ioctl_userfaultfd.2.html,
https://man7.org/linux/man-pages/man2/UFFDIO_API.2const.html.

#### Creating the fd [verified]
- `syscall(SYS_userfaultfd, O_CLOEXEC | O_NONBLOCK | UFFD_USER_MODE_ONLY)`.
  `UFFD_USER_MODE_ONLY = 1`.
  - **Any user** can create a uffd that has `UFFD_USER_MODE_ONLY`. It traps
    only faults that start in user mode.
  - To also trap **kernel-mode** faults (for example `read(2)` into the range,
    `get_user_pages` from `cudaHostRegister`/`ibv_reg_mr`, or `O_DIRECT` DMA
    into the range) the process needs `CAP_SYS_PTRACE` or
    `vm.unprivileged_userfaultfd = 1`. The default is **0**.
- `/dev/userfaultfd` + `ioctl(devfd, USERFAULTFD_IOC_NEW, flags)`
  (`USERFAULTFD_IOC = 0xAA`, `USERFAULTFD_IOC_NEW = _IO(0xAA, 0x00)`). Normal
  file permissions control access to it. A user who has access *can* trap
  kernel faults, and `vm.unprivileged_userfaultfd` is ignored. Added in Linux
  6.1 [recall].
- **Implication for ddramnvme:** default to `UFFD_USER_MODE_ONLY`. Document
  that a transparent range must not be handed to syscalls, `cudaHostRegister`
  or GPU DMA unless its pages are resident (pinned through the explicit API)
  or the process has kernel-fault rights. With `USER_MODE_ONLY` such
  accesses fail with `EFAULT` instead of being serviced.
- Containers: Docker's default seccomp profile blocks `io_uring_*`
  (https://docs.docker.com/engine/security/seccomp/). The same profile is
  understood to allow `userfaultfd` only with `CAP_SYS_PTRACE` [recall —
  verify]. Detect `EPERM`/`ENOSYS` and fall back to the `mprotect`+`SIGSEGV`
  pager (section 2.2 works on Linux too).

#### Handshake: `UFFDIO_API` [verified]
```c
struct uffdio_api { __u64 api; /* = UFFD_API (0xAA) */ __u64 features; __u64 ioctls; };
```
Call it once, first, with the features you require. The kernel fails the call
if a requested feature is unsupported. It writes back the supported set and
`ioctls` (a bitmask of `1<<_UFFDIO_*`).

Feature bits in `include/uapi/linux/userfaultfd.h` (versions are
[recall]/man-page values except where marked):

| bit | name | use for ddramnvme |
|---|---|---|
| 0 | `UFFD_FEATURE_PAGEFAULT_FLAG_WP` | WP fault flag (anonymous WP, 5.7) |
| 1–3 | `EVENT_FORK`, `EVENT_REMAP`, `EVENT_REMOVE` | `EVENT_REMOVE` reports `MADV_DONTNEED`/`MADV_REMOVE` by others on the range |
| 4,5 | `MISSING_HUGETLBFS`, `MISSING_SHMEM` | |
| 6 | `EVENT_UNMAP` | detect munmap of the managed range |
| 7 | `UFFD_FEATURE_SIGBUS` | no message; the faulting thread gets `SIGBUS` (in-thread handling, 4.14) |
| 8 | `THREAD_ID` | `msg.arg.pagefault.feat.ptid` |
| 9,10 | `MINOR_HUGETLBFS` (5.13), `MINOR_SHMEM` (5.14) | MINOR mode |
| 11 | `EXACT_ADDRESS` | unmasked fault address (5.18) |
| 12 | `WP_HUGETLBFS_SHMEM` | WP on shmem/hugetlb (5.19) |
| 13 | `WP_UNPOPULATED` | WP applies to empty PTEs (anonymous, 6.4) |
| 14 | `POISON` | `UFFDIO_POISON` (6.6) |
| 15 | `UFFD_FEATURE_WP_ASYNC` | kernel resolves WP faults itself and records dirtiness (6.7, confirmed by the man page https://man7.org/linux/man-pages/man2/UFFDIO_API.2const.html) |
| 16 | `UFFD_FEATURE_MOVE` | `UFFDIO_MOVE` zero-copy page move (6.8) |
| 17,18 | `UFFD_FEATURE_RWP`, `RWP_ASYNC` | **new in mainline master (v7.3-rc)**: read+write protection tracking, which gives access/working-set tracking for cold-page eviction. Needs `CONFIG_USERFAULTFD_RWP`. Treat as optional. |

#### Registration: `UFFDIO_REGISTER` [verified]
```c
struct uffdio_range    { __u64 start, len; };                    // page-aligned
struct uffdio_register { struct uffdio_range range; __u64 mode; __u64 ioctls; /*out*/ };
UFFDIO_REGISTER_MODE_MISSING = 1<<0  // fault on a not-present page (anon: never touched or zapped)
UFFDIO_REGISTER_MODE_WP      = 1<<1  // fault on write to a uffd-write-protected page
UFFDIO_REGISTER_MODE_MINOR   = 1<<2  // page exists in page cache (shmem/hugetlbfs) but PTE missing
UFFDIO_REGISTER_MODE_RWP     = 1<<3  // (7.3-rc) access to an RW-protected present PTE
```
- `MISSING` is the core pager mode. Use it on a `MAP_PRIVATE|MAP_ANONYMOUS|
  MAP_NORESERVE` range and call `madvise(MADV_NOHUGEPAGE)` so faults stay at
  4 KiB granularity. Use hugetlbfs if 2 MiB pages are wanted.
- `MINOR` needs shmem (memfd) or hugetlbfs. It allows a **two-view design**: the
  app view and a private alias view of the same memfd. Fill the page through
  the alias (even `O_DIRECT` straight into it), then call `UFFDIO_CONTINUE`
  to map it. No memcpy.
- `WP` can be combined with `MISSING` (`MISSING|WP`) on the same range.

#### Reading events [verified]
`read(uffd, &msg, sizeof(struct uffd_msg))`, or `poll`/`epoll` on it. You can
also add a `IORING_OP_READ` on the uffd to io_uring so faults and I/O
completions share one loop.
```c
struct uffd_msg { __u8 event; __u8 r1; __u16 r2; __u32 r3;
  union { struct { __u64 flags, address; union { __u32 ptid; } feat; } pagefault;
          struct { __u32 ufd; } fork; struct { __u64 from, to, len; } remap;
          struct { __u64 start, end; } remove; ... } arg; } __packed;   // 32 bytes
UFFD_EVENT_PAGEFAULT 0x12, FORK 0x13, REMAP 0x14, REMOVE 0x15, UNMAP 0x16
UFFD_PAGEFAULT_FLAG_WRITE 1<<0, _WP 1<<1, _MINOR 1<<2, _RWP 1<<3
```

#### Resolving faults [verified]
```c
struct uffdio_copy     { __u64 dst, src, len, mode; __s64 copy; /*out: bytes or -errno*/ };
  UFFDIO_COPY_MODE_DONTWAKE 1<<0, UFFDIO_COPY_MODE_WP 1<<1   // map the page write-protected (clean)
struct uffdio_zeropage { struct uffdio_range range; __u64 mode; __s64 zeropage; };
  UFFDIO_ZEROPAGE_MODE_DONTWAKE 1<<0                          // for never-written pages
struct uffdio_continue { struct uffdio_range range; __u64 mode; __s64 mapped; };
  UFFDIO_CONTINUE_MODE_DONTWAKE 1<<0, UFFDIO_CONTINUE_MODE_WP 1<<1
struct uffdio_writeprotect { struct uffdio_range range; __u64 mode; };
  UFFDIO_WRITEPROTECT_MODE_WP 1<<0 (set=protect, clear=unprotect+wake), _DONTWAKE 1<<1
struct uffdio_move { __u64 dst, src, len, mode; __s64 move; };
  UFFDIO_MOVE_MODE_DONTWAKE 1<<0, UFFDIO_MOVE_MODE_ALLOW_SRC_HOLES 1<<1
struct uffdio_poison { struct uffdio_range range; __u64 mode; __s64 updated; }  // -> SIGBUS on I/O error
UFFDIO_WAKE (struct uffdio_range)
```
- All resolve ioctls are atomic: a page is never visible half-populated. By
  default they wake the waiters on the range [verified, userfaultfd.rst].
- Errors to handle: `EEXIST` (another thread already populated the page, which
  is benign) and `EAGAIN` (the mm changed, so retry; `copy` holds partial
  progress) [recall, man page].
- **Fill path options:**
  1. `pread`/io_uring into a staging buffer, then `UFFDIO_COPY` (one memcpy in
     the kernel). This is the simplest and proven path (QEMU postcopy,
     Firecracker snapshot restore).
  2. io_uring `O_DIRECT` read into a page-aligned **anonymous** staging page,
     then `UFFDIO_MOVE` (6.8+) into place. Zero copy, but the source must be
     exclusively mapped private anon memory.
  3. memfd + `MINOR`: `O_DIRECT` into the alias view, then `UFFDIO_CONTINUE`.
     Zero copy.
- Use `UFFDIO_COPY_MODE_WP` when filling. A page that comes in clean then
  produces exactly one WP fault on its first write (dirty tracking, §4).
- `UFFD_FEATURE_SIGBUS` variant: no monitor thread. The faulting thread gets
  `SIGBUS` and can issue `UFFDIO_COPY` from the handler (ioctl is a raw
  syscall). This gives lower latency for synchronous faults but makes the
  handler async-signal-context code. Keep it as an option and do not make it
  the default.

#### Eviction [verified semantics]
- On an anonymous private range registered with `MISSING`,
  `madvise(addr, len, MADV_DONTNEED)` zaps the PTEs and frees the pages. The
  **next access raises a new MISSING fault**, so the range is re-armed with no
  re-registration. (`MADV_REMOVE`/`fallocate(PUNCH_HOLE)` is the equivalent
  for memfd/shmem.) `UFFD_FEATURE_EVENT_REMOVE` would report these to the
  monitor. Our own `DONTNEED` calls do not need that.
- Safe dirty-page eviction sequence (avoids losing a concurrent write):
  1. `UFFDIO_WRITEPROTECT(range, MODE_WP)` with **sync** WP. Writers now block
     in a WP fault.
  2. Write back the page (io_uring `O_DIRECT` write from the page itself; it is
     page-aligned).
  3. `madvise(MADV_DONTNEED)`.
  4. Wake any blocked writers with `UFFDIO_WAKE`. They re-fault as MISSING and
     get the fresh copy.
- With `WP_ASYNC`, note that `MADV_DONTNEED` counts as "dirtying" in async
  mode [verified, userfaultfd.rst]. Re-arm with `UFFDIO_WRITEPROTECT` after
  each fill.
- RWP (7.3-rc) state is also lost on `MADV_DONTNEED`, so re-arm with
  `UFFDIO_RWPROTECT` [verified].

### 1.2 io_uring + O_DIRECT

- Crate: `io-uring` (tokio-rs) 0.7.x, MIT OR Apache-2.0 [verified, Cargo.toml].
- Recommended setup: one ring per I/O worker. Use `IORING_SETUP_SINGLE_ISSUER`
  and `IORING_SETUP_COOP_TASKRUN`, plus optional `SQPOLL` for dedicated
  cores. Register files (`register_files`) and buffers (`register_buffers`,
  then `ReadFixed`/`WriteFixed`) for the staging slab. `IORING_OP_READ` on the
  uffd fd lets fault events join the same completion loop [recall].
- O_DIRECT alignment: query `statx(fd, ..., STATX_DIOALIGN)` (Linux 6.1) to get
  `stx_dio_mem_align` (buffer address alignment) and `stx_dio_offset_align`
  (file offset and length alignment). Newer kernels add
  `STATX_DIO_READ_ALIGN` / `stx_dio_read_offset_align`, a separate,
  possibly smaller read alignment [verified, include/uapi/linux/stat.h].
  Fallback is the logical block size (`ioctl(BLKSSZGET)`), typically 512 or
  4096. **Practical rule: align address, offset and length to 4096** (page
  size), which satisfies every NVMe LBA format up to 4 KiB.
- `kernel.io_uring_disabled` sysctl: 0 = allowed, 1 = only `io_uring_group`
  members or privileged processes, 2 = disabled. `io_uring_setup` returns
  `-EPERM` [verified, Documentation/admin-guide/sysctl/kernel.rst]. Docker's
  default seccomp blocks `io_uring_setup/enter/register`
  (https://docs.docker.com/engine/security/seccomp/). **A fallback engine is
  mandatory:** a thread pool with `preadv2`/`pwritev2` (`RWF_HIPRI` optional)
  on `O_DIRECT` fds.

### 1.3 NVIDIA GPUDirect Storage (cuFile)

Sources: `cufile.h(3)` (https://manpages.ubuntu.com/manpages/noble/man3/cufile.h.3.html),
API reference https://docs.nvidia.com/gpudirect-storage/api-reference-guide,
getting started https://docs.nvidia.com/gpudirect-storage/getting-started/index.html.

API [verified, man page]:
```c
CUfileError_t cuFileDriverOpen(void);           // init lib, open nvidia-fs (or P2PDMA path)
CUfileError_t cuFileDriverClose(void);          // (_v2 also exists)
CUfileError_t cuFileHandleRegister(CUfileHandle_t *fh, CUfileDescr_t *descr);
   // descr.type = CU_FILE_HANDLE_TYPE_OPAQUE_FD (1); descr.handle.fd = fd opened with O_DIRECT
   //   (OPAQUE_WIN32 = 2 and USERSPACE_FS = 3 also exist in the enum)
   // errors: CU_FILE_INVALID_FILE_OPEN_FLAG if no O_DIRECT, CU_FILE_IO_NOT_SUPPORTED for unsupported FS
void          cuFileHandleDeregister(CUfileHandle_t fh);
CUfileError_t cuFileBufRegister(const void *devPtr_base, size_t length, int flags); // pin BAR1 mapping, reused staging buffers
CUfileError_t cuFileBufDeregister(const void *devPtr_base);
ssize_t cuFileRead (CUfileHandle_t fh, void *devPtr_base, size_t size, off_t file_offset, off_t devPtr_offset);
ssize_t cuFileWrite(CUfileHandle_t fh, const void *devPtr_base, size_t size, off_t file_offset, off_t devPtr_offset);
   // return bytes; -1 + errno for FS errors; other negatives = -CUfileOpError
// batch / async: cuFileBatchIOSetUp(&batch, nr), cuFileBatchIOSubmit(batch, nr, CUfileIOParams_t*, flags),
//   cuFileBatchIOGetStatus(batch, min_nr, &nr, CUfileIOEvents_t*, timespec*), cuFileBatchIODestroy;
//   cuFileReadAsync/cuFileWriteAsync + cuFileStreamRegister (stream-ordered, CUDA 12.2+) [recall]
// tuning: cuFileDriverSetPollMode, SetMaxDirectIOSize, SetMaxCacheSize, SetMaxPinnedMemSize
```
- A device buffer that has **not** been registered still works: the library
  bounces through internal pinned buffers. The man page recommends this for
  unaligned offsets/sizes or when BAR1 is smaller than the allocation
  [verified].
- **Compatibility mode:** `cufile.json` sets `"properties.allow_compat_mode":
  true`. When true DMA is unavailable (no `nvidia-fs`/P2PDMA, unsupported FS,
  or no `O_DIRECT`), cuFile falls back to POSIX `pread` into host bounce
  buffers plus `cudaMemcpy`. Same API, lower throughput. A
  `force_compat_mode` key also exists. Point the library at a custom config
  with `CUFILE_ENV_PATH_JSON` [recall]. From CUDA 12.8, GDS can use upstream
  Linux **P2PDMA** with no `nvidia-fs.ko` or patched NVMe driver, given kernel,
  GPU and driver prerequisites (getting-started guide).
- License: libcufile ships with the CUDA toolkit under the NVIDIA proprietary
  EULA. Load it with `dlopen`, never vendor it. `cudarc` (MIT OR Apache-2.0
  [verified]) has a `cufile` feature with bindings [verified, Cargo.toml].
  KvikIO (RAPIDS, Apache-2.0 [recall]) is a good reference for the
  "try GDS, else compat" logic.
- Linux only. On Windows there is no GDS for consumer GPUs; use DirectStorage
  instead.

---

## 2. macOS

### 2.1 Facts

- **No userfaultfd** and no equivalent kernel pager API is available to apps.
- **Page size is 16 KiB on Apple Silicon** (`vm_page_size`,
  `sysconf(_SC_PAGESIZE)`, or `getpagesize()`); XNU ARM uses
  `PAGE_SHIFT_CONST` 14 [verified, xnu `osfmk/mach/arm/vm_param.h`]. Intel Macs
  use 4 KiB. **Never hard-code; read `vm_page_size` at runtime.** Rosetta
  processes still see 16 KiB VM granularity [recall].
- **Signal mapping** [verified, xnu `bsd/uxkern/ux_exception.c`]: an
  `EXC_BAD_ACCESS` with code `KERN_INVALID_ADDRESS` becomes `SIGSEGV`.
  **Every other code, including `KERN_PROTECTION_FAILURE` from touching a
  `PROT_NONE` page, becomes `SIGBUS`.** So an `mprotect` pager on macOS
  **must handle SIGBUS** (and SIGSEGV for safety). Wasmtime does the same:
  "On Darwin, guard page accesses are raised as SIGBUS"
  ([wasmtime signals.rs](https://github.com/bytecodealliance/wasmtime/blob/main/crates/wasmtime/src/runtime/vm/sys/unix/signals.rs)).

### 2.2 Transparent-mode options

**Option A: `mprotect(PROT_NONE)` + `sigaction(SIGBUS/SIGSEGV, SA_SIGINFO|SA_ONSTACK)`**
- Address is `si_addr`. Read vs write comes from the ucontext ESR on arm64:
  `uc_mcontext->__es.__esr`, WnR bit 6 [recall].
- Async-signal-safety: POSIX lists `pread`, `read`, `write`, `sem_post` as
  safe. `mprotect`, `mach_vm_*`, `malloc`, and `pthread_mutex_*` are **not**
  on the list (though `mprotect` is a plain syscall and works in practice,
  as in libsigsegv and many GCs). Rules for the handler: no allocation, no
  locks that ordinary code can hold, preallocated per-thread state, a
  `sigaltstack` on every thread (Rust std installs one only on the main
  thread and on threads it spawns, per Wasmtime's notes).
- Better pattern: the handler only signals a fault-service thread through a
  lock-free queue plus `write()` to a pipe or `semaphore_signal`, then blocks
  in `semaphore_wait`/`read` until the page is mapped, then returns and the
  access retries.
- Downside: signal handlers are process-global and conflict with other
  runtimes (JVM, Mono, crash reporters), so previous handlers must be chained.

**Option B (recommended): Mach exception ports**
- `task_set_exception_ports(mach_task_self(), EXC_MASK_BAD_ACCESS, port,
  EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES, THREAD_STATE_NONE)` is task-wide
  and catches every thread, including ones we did not create. Use
  `task_swap_exception_ports` or `task_get_exception_ports` to save the
  previous handler and forward faults that are not ours. `thread_set_exception_ports`
  covers one thread; Wasmtime uses it with `EXC_MASK_BAD_ACCESS |
  EXC_MASK_BAD_INSTRUCTION | EXC_MASK_ARITHMETIC` and `EXCEPTION_DEFAULT |
  MACH_EXCEPTION_CODES` in `lazy_per_thread_init` [verified,
  [wasmtime machports.rs](https://github.com/bytecodealliance/wasmtime/blob/main/crates/wasmtime/src/runtime/vm/sys/unix/machports.rs)].
- The faulting thread is **suspended by the kernel**. A dedicated handler
  thread `mach_msg`-receives `mach_exception_raise` (`code[0]` = kern_return,
  `code[1]` = fault address). It does ordinary, non-signal-context work: I/O,
  locks, allocation, `mach_vm_*`. It then replies `KERN_SUCCESS` and the
  thread re-executes the instruction. Replying `KERN_FAILURE` lets the kernel
  convert the exception into the BSD signal (the SIGBUS above).
  **Async-signal-safety stops being a concern.**
- Caveats: the Wasmtime source notes that mach ports do not survive `fork()`
  (the child must re-init or refuse). Debuggers (lldb) intercept
  `EXC_BAD_ACCESS` first, so set `process handle` or skip transparent mode
  under a debugger. Use the `mach2` crate (Wasmtime does), which is
  BSD-2-Clause OR MIT OR Apache-2.0 [verified]. Wasmtime's file is credited
  as "lifted from SpiderMonkey", so treat it as a reference, not code to copy.
  (Wasmtime is Apache-2.0 [verified], with LLVM-exception [recall].)

**Atomic page fill on macOS (no `UFFDIO_COPY` equivalent).** Calling
`mprotect(RW)`, then filling, then `mprotect(R)` exposes a half-filled page to
other threads. Alternatives:
1. Staging plus remap: read into a private staging page, then
   `mach_vm_remap(mach_task_self(), &dst, size, 0, VM_FLAGS_FIXED |
   VM_FLAGS_OVERWRITE, mach_task_self(), staging, FALSE /*copy*/, &cur,
   &max, VM_INHERIT_NONE)`. This atomically replaces the `PROT_NONE` page.
   Then deallocate or reuse the staging VA [recall: verify OVERWRITE
   semantics in a prototype].
2. Dual view: back the arena with `mach_make_memory_entry_64` (or
   `shm_open`/`memfd`-like object) mapped twice: a hidden RW alias plus the
   app view. Fill through the alias, then `mprotect` the app view page to
   R/RW. This mirrors the Linux MINOR design.
- Granularity: 16 KiB on Apple Silicon. The transparent-mode page unit must
  be `max(vm_page_size, 16 KiB)`.

**Eviction on macOS:** write back if dirty, then `mprotect(PROT_NONE)`, then
`madvise(MADV_FREE_REUSABLE)` (value 7; freed pages stop counting toward the
footprint), or `mmap(MAP_FIXED|MAP_ANON|MAP_PRIVATE, PROT_NONE)` over the
range to drop pages hard. Re-arming happens through the protection. Other
madvise values: `MADV_FREE` 5, `MADV_FREE_REUSE` 8, `MADV_ZERO` 11 [verified,
xnu `bsd/sys/mman.h`].

### 2.3 File I/O on macOS [verified constants, xnu `bsd/sys/fcntl.h`]
- `fcntl(fd, F_NOCACHE, 1)` (48) bypasses the unified buffer cache, so this is
  macOS "direct I/O". Aligned I/O (4 KiB, or page-aligned buffers) takes the
  direct path. Unaligned I/O still works but is slow [recall].
  `F_GLOBAL_NOCACHE` (55) applies to the whole file.
- `fcntl(fd, F_RDADVISE, &(struct radvisory){ .ra_offset = off, .ra_count =
  (int)len })` (44) issues an async readahead into the cache. Use it only for
  buffered prefetch.
- `fcntl(fd, F_PREALLOCATE, &(fstore_t){ .fst_flags = F_ALLOCATECONTIG |
  F_ALLOCATEALL, .fst_posmode = F_PEOFPOSMODE, .fst_offset = 0, .fst_length =
  n })` (42) preallocates the swap/backing file, falling back to
  `F_ALLOCATEALL` alone. Follow with `ftruncate`. Fields: `fst_flags`,
  `fst_posmode`, `fst_offset`, `fst_length`, `fst_bytesalloc` (out).
- Other useful fcntls: `F_PUNCHHOLE` (99) to free backing blocks of discarded
  pages, `F_BARRIERFSYNC` (85) and `F_FULLFSYNC` (51) for durability, and
  `F_LOG2PHYS` (49).
- **There is no io_uring analogue.** Engine: a thread pool doing
  `pread`/`pwrite` on `F_NOCACHE` fds, 2–4 threads per NVMe queue depth
  target. `dispatch_io` is an alternative. POSIX `aio_*` is limited by
  `kern.aiomax` and is not recommended [recall].
- `mach_vm_allocate(mach_task_self(), &addr, size, VM_FLAGS_ANYWHERE)` or
  `mmap(MAP_ANON|MAP_PRIVATE|MAP_NORESERVE, PROT_NONE)` reserves the big
  virtual range. Both are lazily committed.

### 2.4 GPU side (Metal)
- **Unified memory** on Apple Silicon: `MTLDevice.hasUnifiedMemory`. The GPU
  shares system memory, and `MTLStorageModeShared` is the default for buffers
  and textures on Apple silicon [verified, docs
  https://developer.apple.com/documentation/metal/mtldevice/hasunifiedmemory,
  https://developer.apple.com/documentation/metal/mtlstoragemode/shared]. On
  macOS the "DRAM tier" and the GPU tier are therefore the same pages. Upload
  is a residency problem, not a copy.
- `newBufferWithBytesNoCopy:length:options:deallocator:` (macOS 10.11+) needs
  "a **page-aligned** pointer" and a length "that results in a page-aligned
  region". The allocation "needs to exist within a single virtual memory (VM)
  region" [verified,
  https://developer.apple.com/documentation/metal/mtldevice/makebuffer(bytesnocopy:length:options:deallocator:)].
  Page here means `vm_page_size` (16 KiB). **Wrap only resident and locked
  ranges.** GPU accesses do not go through our CPU exception handler, and the
  kernel wiring of a non-resident `PROT_NONE` page will not be serviced by a
  user-mode pager. Before wrapping a transparent range: prefetch, mark it
  pinned, and `mprotect` it RW.
- **Metal 3 fast resource loading:** `MTLIOCommandQueue`, `MTLIOFileHandle`,
  `MTLIOCommandBuffer` (macOS 13 / iOS 16) [verified,
  https://developer.apple.com/documentation/metal/mtliocommandqueue,
  https://developer.apple.com/documentation/metal/resource-loading].
  - `device.newIOCommandQueueWithDescriptor:` (`MTLIOCommandQueueDescriptor`:
    `type` concurrent/serial, `priority`, `maxCommandsInFlight`,
    `maxCommandBufferCount`).
  - `device.newIOHandleWithURL:compressionMethod:error:`, where
    `MTLIOCompressionMethod` ∈ {zlib, lzfse, lz4, lzma, lzBitmap}. Compressed
    files are produced with `MTLIOCreateCompressionContext`/
    `MTLIOCompressionContextAppendData` [recall].
  - Command buffer: `loadBuffer:offset:size:sourceHandle:sourceHandleOffset:`,
    `loadTexture:slice:level:size:sourceBytesPerRow:sourceBytesPerImage:destinationOrigin:sourceHandle:sourceHandleOffset:`,
    `loadBytes:size:sourceHandle:sourceHandleOffset:` (CPU memory),
    `signalEvent:value:` / `waitForEvent:value:` (`MTLSharedEvent` sync with
    render queues), `addCompletedHandler:`, `commit`, `tryCancel`, `status`.
  - This is the macOS counterpart of DirectStorage. Use it for the **explicit
    stream API** when the destination is an `MTLBuffer`/`MTLTexture`.
- `MTLHeap`: sub-allocate resources. Resources in a heap share storage and CPU
  cache mode. **Heaps can be purgeable** (`setPurgeableState:`), but
  individual resources only reflect the heap's state [verified,
  https://developer.apple.com/documentation/metal/mtlheap]. A good backing
  for a GPU-side cache whose eviction the OS may trigger. Check
  `MTLPurgeableStateEmpty` before use.
- Rust: `objc2-metal` (objc2 workspace: Zlib OR Apache-2.0 OR MIT
  [verified]). The `metal` crate (MIT OR Apache-2.0) is **deprecated** in
  favour of objc2 [verified, metal-rs README].

---

## 3. Windows

Sources: the `MicrosoftDocs/sdk-api` docs branch (canonical
`https://learn.microsoft.com/windows/win32/api/<header>/<page>`), for example
https://learn.microsoft.com/windows/win32/api/memoryapi/nf-memoryapi-virtualalloc2.

### 3.1 Reserving and committing memory [verified]
- `VirtualAlloc2(Process, BaseAddress, Size, AllocationType, PageProtection,
  ExtendedParameters, ParameterCount)`: Windows 10 / Server 2016 and later,
  `onecore.lib`/KernelBase.dll.
  - Flags include `MEM_RESERVE`, `MEM_COMMIT`, `MEM_RESERVE_PLACEHOLDER`,
    `MEM_REPLACE_PLACEHOLDER`, `MEM_LARGE_PAGES`, `MEM_WRITE_WATCH`.
    `MEM_ADDRESS_REQUIREMENTS` sets alignment. A non-null base must be
    64 KiB-aligned (allocation granularity).
- Placeholders: `VirtualAlloc2(MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
  PAGE_NOACCESS)`. Split with `VirtualFree(p, size, MEM_RELEASE |
  MEM_PRESERVE_PLACEHOLDER)`, coalesce with `MEM_COALESCE_PLACEHOLDERS`.
  Replace with `MapViewOfFile3(section, process, base, offset, size,
  MEM_REPLACE_PLACEHOLDER, prot, NULL, 0)`. **With `MEM_REPLACE_PLACEHOLDER`
  the 64 KiB alignment of Offset/BaseAddress does not apply** (page alignment
  only) [verified, nf-memoryapi-mapviewoffile3]. Unmap back to a placeholder
  with `UnmapViewOfFile2(process, base, MEM_PRESERVE_PLACEHOLDER)`
  [verified].
- `VirtualProtect` works at 4 KiB page granularity on private memory and on
  mapped views.

### 3.2 Fault-driven pager: Vectored Exception Handler
- `AddVectoredExceptionHandler(First=1, handler)`. The handler receives
  `EXCEPTION_POINTERS*`. Check `ExceptionRecord->ExceptionCode ==
  EXCEPTION_ACCESS_VIOLATION` (0xC0000005). `ExceptionInformation[0]` is
  0 = read, 1 = write, 8 = DEP. `ExceptionInformation[1]` is the address
  [verified, ns-winnt-exception_record]. `EXCEPTION_IN_PAGE_ERROR` has the same
  layout plus NTSTATUS in `[2]`.
- After fixing the page, return `EXCEPTION_CONTINUE_EXECUTION` (-1). The
  instruction re-executes. Otherwise return `EXCEPTION_CONTINUE_SEARCH`.
- VEH runs **on the faulting thread** in normal user mode, not a signal
  context. Locks, I/O and waits are allowed, but beware of re-entrancy (a
  fault inside the handler) and of the loader lock.
- `PAGE_GUARD` (`STATUS_GUARD_PAGE_VIOLATION`, 0x80000001) is one-shot and
  auto-clears. It suits access sampling (an LRU "accessed" bit) but not the
  pager, because the page must already be committed and filled.
- **Atomic fill** (the main design question). With private memory, committing
  `PAGE_READWRITE` and then filling exposes partial data to other threads.
  Recommended: **placeholder + per-chunk section**:
  1. Reserve the whole virtual range as one placeholder and split it into
     chunk-sized placeholders lazily, 64 KiB default (chunk = transparent page
     unit).
  2. On fault: `CreateFileMapping2(INVALID_HANDLE_VALUE /*pagefile*/, …,
     PAGE_READWRITE, SEC_COMMIT, chunk)`. Map a temporary RW alias view
     (`MapViewOfFile3`, system-chosen address). Fill it with an unbuffered
     read directly into the alias, which is page-aligned and so
     sector-aligned. Then `MapViewOfFile3(section, …, chunk_base, 0, chunk,
     MEM_REPLACE_PLACEHOLDER, PAGE_READONLY or PAGE_READWRITE)`,
     `UnmapViewOfFile(alias)`, `CloseHandle(section)` (the view keeps it
     alive). Other threads see either the placeholder (they fault and wait on
     the per-chunk event) or the full page. **Atomic.**
  3. Evict: write back if dirty, then `UnmapViewOfFile2(…,
     MEM_PRESERVE_PLACEHOLDER)`. The memory is freed when the last view goes,
     and the VA becomes a placeholder again, so the next touch is an AV and
     the range is re-armed.
  - Cost: one section and one VAD per resident chunk. Keep chunks ≥ 64 KiB.
    Coalesce placeholders when ranges are freed.
  - Alternative for small/medium ranges: one pagefile-backed section mapped
    twice (app view + RW alias). Fill via the alias, then
    `VirtualProtect(app page, PAGE_READONLY)`. Evict with
    `VirtualProtect(NOACCESS)` plus `DiscardVirtualMemory` on the alias
    (requires `PAGE_READWRITE` [verified]) or `VirtualAlloc(MEM_RESET)`, which
    is allowed on pagefile-backed shared views but not on file-mapped views
    [verified, nf-memoryapi-virtualalloc]. **Downside: the whole section is
    commit-charged**, so it does not scale to TB-sized virtual ranges.
  - Alternative: AWE (`AllocateUserPhysicalPages` / `MapUserPhysicalPages` on
    a `MEM_PHYSICAL` region). Fast atomic page-granular remap, but needs
    `SeLockMemoryPrivilege`. Optional "server mode" [recall].
- Eviction primitives summary:
  - `VirtualFree(MEM_DECOMMIT)`: private memory only. Returns commit charge.
  - `DiscardVirtualMemory(addr, size)`: keeps commit, content undefined, needs
    `PAGE_READWRITE` [verified].
  - `OfferVirtualMemory(addr, size, VMOfferPriority*)` +
    `ReclaimVirtualMemory`: pages become inaccessible and are not written to
    the pagefile. Useful for a **clean cache tier the OS may reclaim**
    [verified].
  - `VirtualAlloc(MEM_RESET)` / `MEM_RESET_UNDO`.

### 3.3 Unbuffered I/O, IOCP, IoRing
- `CreateFileW(..., FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED, ...)`.
  Offsets and sizes must be multiples of the **volume sector size**, and
  buffers **should be aligned to the physical sector size** (page-aligned
  `VirtualAlloc` memory satisfies this). Get the physical sector size from
  `IOCTL_STORAGE_QUERY_PROPERTY` (`STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR`) or
  `GetFileInformationByHandleEx(FileStorageInfo)` [verified,
  https://learn.microsoft.com/windows/win32/fileio/file-buffering].
  **Rule: 4096-align everything.**
- IOCP: `CreateIoCompletionPort(file, iocp, key, 0)` +
  `ReadFile(…, &OVERLAPPED)` + `GetQueuedCompletionStatusEx`. Works on every
  Windows version and is the fallback engine. Use
  `SetFileCompletionNotificationModes(FILE_SKIP_COMPLETION_PORT_ON_SUCCESS)`.
- **IoRing** (`ioringapi.h`, Windows 11 build 22000+) [verified]:
  - `QueryIoRingCapabilities(&caps)` (cached per process). Then
    `CreateIoRing(IORING_VERSION, IORING_CREATE_FLAGS{required, advisory},
    sqSize, cqSize, &hring)`. Sizes round up to a power of two, and the CQ is
    at least twice the SQ.
  - Register resources with `BuildIoRingRegisterFileHandles` and
    `BuildIoRingRegisterBuffers(hring, count, IORING_BUFFER_INFO*,
    userData)`. Queue reads with `BuildIoRingReadFile(hring,
    IORING_HANDLE_REF, IORING_BUFFER_REF, bytes, fileOffset, userData,
    IORING_SQE_FLAGS)`, which returns `IORING_E_SUBMISSION_QUEUE_FULL` when
    the SQ is full.
  - Submit with `SubmitIoRing(hring, waitOperations, milliseconds,
    &submitted)` and collect with `PopIoRingCompletion(hring, &IORING_CQE)`,
    which returns S_FALSE when empty. Close with `CloseIoRing` (not
    CloseHandle). `SetIoRingCompletionEvent` is available for waits.
  - Writes and flushes (`BuildIoRingWriteFile`, `BuildIoRingFlushFile`) were
    added in Windows 11 22H2 per
    https://windows-internals.com/one-year-to-i-o-ring-what-changed/. They
    are not in the official doc pages read here, so check
    `IsIoRingOpSupported(IORING_OP_WRITE)` at runtime. **Reads via IoRing,
    writes via IOCP is a safe split.**
- **Engine choice:** IoRing when it is available and supports the op;
  otherwise IOCP.

### 3.4 DirectStorage: the gaming GPU path
Source: https://github.com/microsoft/DirectStorage (samples MIT [verified];
the runtime is the `Microsoft.Direct3D.DirectStorage` NuGet redistributable
under a Microsoft license [recall]; load `dstorage.dll` dynamically, do not
vendor it).
- `DStorageGetFactory(IID_PPV_ARGS(&factory))`, then
  `factory->OpenFile(path, IID_PPV_ARGS(&file))`. Create a queue with
  `DSTORAGE_QUEUE_DESC { Capacity = DSTORAGE_MAX_QUEUE_CAPACITY, Priority,
  SourceType = DSTORAGE_REQUEST_SOURCE_FILE | _MEMORY, Device =
  ID3D12Device* }` and `factory->CreateQueue(&desc, IID_PPV_ARGS(&queue))`
  [verified, HelloDirectStorage.cpp].
- Each `DSTORAGE_REQUEST` has `Options.SourceType`, `Options.DestinationType`
  (`_BUFFER`, `_TEXTURE_REGION`, `_MULTIPLE_SUBRESOURCES`, `_MEMORY` (CPU),
  `_TILES`, `_MULTIPLE_SUBRESOURCES_RANGE`), `Options.CompressionFormat`
  (`DSTORAGE_COMPRESSION_FORMAT_NONE`/`_GDEFLATE`, plus custom formats),
  `Source.File{Source, Offset, Size}`, `Destination`, and
  `UncompressedSize`. Then call `queue->EnqueueRequest(&r)`,
  `queue->EnqueueSignal(fence, value)` (or `EnqueueStatus`), and
  `queue->Submit()`. Check errors with `queue->RetrieveErrorRecord`
  [verified, sample].
- GPU decompression with **GDeflate** (since 1.1). The reference codec, HLSL
  shader and libdeflate fork are in the repo
  ([GDeflate/README](https://github.com/microsoft/DirectStorage/tree/main/GDeflate)).
  **DirectStorage 1.3** added `EnqueueRequests` (batching plus a D3D12 fence).
  **DirectStorage 1.4** (public preview, 2026-03-11) adds **Zstandard** with
  CPU and GPU decompression, plus an open-sourced Zstd GPU decompression
  shader tuned for ≤256 KiB chunks
  (https://devblogs.microsoft.com/directx/directstorage-1-4-release-adds-support-for-zstandard/).
- **DirectStorage is read-only** [verified, README]. Write-back of dirty
  pages always goes through IoRing/IOCP.
- Best request size is about 64 KiB (README guidance). This matches the
  64 KiB Windows chunk choice.
- NVIDIA **RTX IO** is NVIDIA's GPU-decompression implementation underneath
  DirectStorage (GDeflate) [recall]. There is nothing extra to integrate.

### 3.5 Dirty tracking: `MEM_WRITE_WATCH` [verified]
- `VirtualAlloc(MEM_RESERVE | MEM_WRITE_WATCH, …)` is private memory only,
  must include `MEM_RESERVE`, and **cannot be turned off** until the region is
  freed.
- `GetWriteWatch(WRITE_WATCH_FLAG_RESET, base, size, addrs, &count,
  &granularity)` returns the written pages and resets atomically.
  `ResetWriteWatch` alone races with writers.
- **It does not apply to mapped views or placeholders**, so the
  placeholder+section pager uses write-fault tracking (§4).

---

## 4. Dirty-page tracking per OS

| OS | Mechanism | Notes |
|---|---|---|
| Linux ≥ 6.7 | `UFFD_FEATURE_WP_ASYNC` + `UFFDIO_REGISTER_MODE_WP`; fill with `UFFDIO_COPY_MODE_WP`; harvest with `ioctl(pagemap_fd, PAGEMAP_SCAN, …)` asking `PAGE_IS_WRITTEN` + `PM_SCAN_WP_MATCHING` (atomic "get dirty + re-protect") | No fault per write; the kernel clears the bit. Emulates `GetWriteWatch`. Not for hugetlb. `MADV_DONTNEED` counts as dirty ([UFFDIO_API(2const)](https://man7.org/linux/man-pages/man2/UFFDIO_API.2const.html), userfaultfd.rst). |
| Linux 5.7–6.6 | sync uffd WP: `UFFD_PAGEFAULT_FLAG_WP` event → mark dirty → `UFFDIO_WRITEPROTECT(mode=0)` (unprotect + wake) | One round trip per first write per page per epoch. |
| Linux (no uffd) | `mprotect(PROT_READ)` + SIGSEGV → mark dirty → `mprotect(RW)` | Same as macOS. Soft-dirty (`/proc/self/clear_refs` = 4) is process-wide and coarse. Avoid. |
| macOS | Fill read-only (`PROT_READ`). The first write raises `EXC_BAD_ACCESS`/`KERN_PROTECTION_FAILURE` (SIGBUS); the Mach handler marks dirty and `mprotect(RW)`s | 16 KiB granularity. |
| Windows (private memory) | `MEM_WRITE_WATCH` + `GetWriteWatch(WRITE_WATCH_FLAG_RESET)` | Cheapest. Only for `VirtualAlloc` private regions (explicit API arenas). |
| Windows (placeholder views) | Map `PAGE_READONLY`; on write AV (`ExceptionInformation[0]==1`) mark dirty and `VirtualProtect(PAGE_READWRITE)` | |

Write-back protocol on every OS: (1) re-protect the page read-only (uffd WP /
`mprotect` / `VirtualProtect`), (2) write it, (3) clear the dirty bit, then
either keep it clean-resident or evict.

---

## 5. Prior art and how ddramnvme compares

| Project | What it does | Relation to ddramnvme | License (reuse?) |
|---|---|---|---|
| Linux swap / zswap | Kernel anonymous paging to a swap device; zswap compresses pages in RAM before writeback | Transparent mode is "app-controlled swap" with app-specific prefetch, no global LRU interference, and GPU awareness. Borrow zswap's idea of an optional compressed DRAM tier (lz4) | GPL-2.0, ideas only |
| **Umap** (LLNL) | userfaultfd-based user-space mmap/paging library with pluggable stores, fill/evict worker pools, configurable page size | Closest architectural match to transparent mode on Linux | **LGPL-2.1** [verified, https://github.com/LLNL/umap/blob/develop/LICENSE]. **Do not copy code** into an Apache-2.0 crate; design inspiration only |
| FastMap (FORTH, USENIX ATC'20) | Kernel module that optimizes the mmap I/O path for fast storage (scalable page cache, 4.14 kernel) | Shows that kernel mmap is the bottleneck at NVMe speeds, which motivates the user-space pager + io_uring | Kernel module `MODULE_LICENSE("GPL")` [verified, driver/main.c]; ideas only |
| Aquila (FORTH, EuroSys'21 "Memory-mapped I/O on steroids") | Runs the app in non-root ring 0 (VT-x, Dune-like) to handle mmio faults without a kernel round trip | Shows fault-path cost dominates. Not portable (needs virtualization); no public code found | n/a |
| FlexGen / FlexLLMGen | LLM inference offloading weights/KV/activations across GPU/CPU/disk with an LP-based placement policy and zig-zag block schedule | Explicit API use case; its policy could run on top of ddramnvme's pin/prefetch | Apache-2.0 [verified] |
| DeepSpeed ZeRO-Infinity | Offloads params/grads/optimizer state to CPU and NVMe through its async I/O library (libaio, now "DeepNVMe", GDS support) | Explicit API for training; similar staging/pinned-buffer design | Apache-2.0 [verified] |
| llama.cpp mmap | `mmap` of GGUF weights (page cache), optional `mlock`, `--no-mmap` | The baseline we must beat: page cache plus kernel readahead, no control over eviction, 4 KiB faults on first token | MIT [verified] |
| vLLM | PagedAttention KV blocks; CPU swap space / CPU offload (`--cpu-offload-gb`, KV connectors) | KV cache is a natural "explicit stream" user. The block-table abstraction maps well to our page IDs | Apache-2.0 [verified] |
| LMCache | KV cache tiers (GPU → CPU → local disk/NVMe → remote), cross-request reuse; integrates with vLLM | Direct competitor/consumer for the LLM-KV use case. ddramnvme could be its local NVMe backend | Apache-2.0 [verified] |
| Microsoft DirectStorage | Batched NVMe reads → GPU, GDeflate/Zstd GPU decompression | Our Windows GPU upload path (§3.4) | Samples MIT [verified]; runtime redistributable |
| NVIDIA RTX IO | NVIDIA GPU decompression via DirectStorage | Covered by the DirectStorage path | Proprietary |
| Unreal Engine 5 virtual textures (SVT/RVT) and Nanite streaming | Fixed-size tile/page pool on the GPU with feedback-driven requests (GPU writes a feedback buffer of missing pages; CPU streams them asynchronously) | **Key design lesson for GPU transparency:** the GPU cannot fault into our pager, so use a *feedback buffer + page table* pattern for GPU-side transparent mode | Proprietary (UE EULA); ideas only |
| Sony PS5 I/O complex (Kraken) | Hardware Kraken decompressor plus dedicated DMA/coherency engines. Raw SSD ~5.5 GB/s, ~8–9 GB/s typical effective (Cerny, "Road to PS5", 2020) [recall] | Upper-bound reference. Hardware decompression is not available to us; use GPU decompression (GDeflate/Zstd) or CPU lz4 | Proprietary |
| CXL memory tiering: TPP (Meta, ASPLOS'23), Memtis (SOSP'23) | Hot/cold page placement between local DRAM and slower CXL/PMEM. TPP's ideas are upstream (NUMA-balancing memory-tiering mode, `kernel.numa_balancing=2`) [recall]. Memtis uses PEBS sampling + dynamic hot threshold | Hotness classification algorithms (access sampling, histogram threshold) apply directly to our DRAM↔NAND decision. Linux 7.3-rc uffd RWP + `PAGEMAP_SCAN` gives a user-space way to sample accesses | TPP: GPL (kernel). **Memtis: CC BY-NC 4.0** [verified, README], non-commercial, so **ideas only, no code** |
| KvikIO (RAPIDS) | Python/C++ cuFile wrapper with automatic compat fallback and thread pool | Reference for the GDS backend | Apache-2.0 [recall] |
| Wasmtime / SpiderMonkey trap handling | Production-grade signal and Mach-exception handling | Reference for §2.2 | Wasmtime Apache-2.0 [verified] (WITH LLVM-exception [recall]); SpiderMonkey MPL-2.0 [recall], so copying file-level code carries MPL obligations |

---

## 6. Rust crates (licenses verified from each upstream `Cargo.toml`, 2026-10-13)

| Crate | Version seen | License | OS | Use | Apache-2.0 project OK? |
|---|---|---|---|---|---|
| `libc` | 0.2.190 (1.0.0-alpha on main) | MIT OR Apache-2.0 | all | raw syscalls, `mprotect`, `madvise`, `fcntl` | yes |
| `io-uring` | 0.7.15 | MIT OR Apache-2.0 | Linux | I/O engine | yes |
| `userfaultfd` (bytecodealliance/fastly) | 0.9.0 | MIT OR Apache-2.0 | Linux | uffd wrapper (check it covers WP_ASYNC/MOVE; otherwise call ioctls via `libc`) | yes |
| `rustix` | 1.1.5 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | Linux/macOS | safe syscalls, including `userfaultfd` and `statx` | yes |
| `nix` | 0.31.3 | **MIT** | Unix | signals (`sigaction`), `mprotect` | yes (MIT is compatible; keep notice) |
| `windows` | 0.100.0 | MIT OR Apache-2.0 | Windows | COM (D3D12, DirectStorage interfaces if defined there; else hand-written `windows-core` interfaces) | yes |
| `windows-sys` | 0.100.0 | MIT OR Apache-2.0 | Windows | `VirtualAlloc2`, `MapViewOfFile3`, IoRing, VEH, IOCP | yes |
| `mach2` | 0.7.0 | BSD-2-Clause OR MIT OR Apache-2.0 | macOS | exception ports, `mach_vm_*` | yes |
| `objc2-metal` | workspace | Zlib OR Apache-2.0 OR MIT | macOS | Metal, MTLIO* | yes |
| `metal` | 0.33.0 | MIT OR Apache-2.0 | macOS | **deprecated** → use objc2-metal | yes |
| `cudarc` | 0.19.10 | MIT OR Apache-2.0 | Linux/Win | CUDA driver + `cufile` feature (dlopen) | yes |
| `crossbeam` | 0.8.5 | MIT OR Apache-2.0 | all | lock-free queues for fault and I/O dispatch | yes |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 | all | per-page/chunk locks, condvars | yes |
| `lz4_flex` | 0.14.0 | **MIT** | all | pure-Rust LZ4 for the compressed DRAM tier and on-disk pages | yes |
| `zstd` | 0.14.2 | **BSD-3-Clause** (zstd-sys 2.1.1+zstd.1.5.7 also BSD-3-Clause) | all | cold-tier compression | yes. Note that **older `zstd` releases were MIT**; the current main branch is BSD-3-Clause. Upstream libzstd is BSD-3-Clause OR GPL-2.0, so pick BSD |

All are permissive (MIT / BSD / Zlib / Apache) and compatible with an
Apache-2.0 project. Ship a `THIRD-PARTY-NOTICES` and add `cargo-deny`
`[licenses] allow = ["Apache-2.0", "MIT", "BSD-2-Clause", "BSD-3-Clause",
"Zlib", "Apache-2.0 WITH LLVM-exception", "Unicode-3.0"]`. Proprietary
runtimes (libcufile, dstorage.dll, CUDA) are loaded at runtime only and never
vendored.

---

## 7. Recommended architecture per OS

| | **Linux** | **macOS** | **Windows** |
|---|---|---|---|
| Fault mechanism (transparent mode) | `userfaultfd` (`UFFD_USER_MODE_ONLY` by default; `/dev/userfaultfd` when available for kernel-fault coverage), `REGISTER_MODE_MISSING|WP` on anon `MAP_NORESERVE` + `MADV_NOHUGEPAGE`; monitor thread(s) on epoll/io_uring. Fallback: `mprotect`+SIGSEGV | Mach exception port (`task_set_exception_ports`, `EXC_MASK_BAD_ACCESS`, `EXCEPTION_DEFAULT|MACH_EXCEPTION_CODES`) serviced by a handler thread; chain previous ports. Fallback: SIGBUS+SIGSEGV handler | VEH (`AddVectoredExceptionHandler(1, …)`) on `EXCEPTION_ACCESS_VIOLATION` over a placeholder-reserved range |
| Atomic fill | `UFFDIO_COPY` (+`MODE_WP`); zero-copy upgrade: `UFFDIO_MOVE` (6.8+) or memfd+`MINOR`+`UFFDIO_CONTINUE` | Staging page + `mach_vm_remap(VM_FLAGS_FIXED|VM_FLAGS_OVERWRITE)`, or dual-view memory entry + `mprotect` | Per-chunk pagefile section: fill via alias view → `MapViewOfFile3(MEM_REPLACE_PLACEHOLDER)` |
| I/O engine | `io-uring` with registered files/buffers, `O_DIRECT`; fallback thread pool `preadv2/pwritev2` (seccomp/`io_uring_disabled`) | Thread pool `pread/pwrite` on `F_NOCACHE` fds; `F_PREALLOCATE` the backing file; `F_RDADVISE` only for buffered prefetch | IoRing (Win11 22000+) for reads; IOCP + `FILE_FLAG_NO_BUFFERING|FILE_FLAG_OVERLAPPED` for writes and on Win10 |
| Dirty tracking | ≥6.7: `WP_ASYNC` + `PAGEMAP_SCAN(PAGE_IS_WRITTEN, WP_MATCHING)`; else sync uffd-WP faults | Fill `PROT_READ`; first write → exception → mark dirty → `mprotect(RW)` | Placeholder views: map `PAGE_READONLY`, write-AV → `VirtualProtect(RW)`. Explicit-API private arenas: `MEM_WRITE_WATCH` + `GetWriteWatch(RESET)` |
| Eviction call | WP-protect → write back → `madvise(MADV_DONTNEED)` (re-arms MISSING) → `UFFDIO_WAKE` | `mprotect(PROT_NONE)` → write back → `madvise(MADV_FREE_REUSABLE)` or `mmap(MAP_FIXED, PROT_NONE)` | `VirtualProtect(READONLY)` → write back → `UnmapViewOfFile2(MEM_PRESERVE_PLACEHOLDER)` + close section. Private arenas: `VirtualFree(MEM_DECOMMIT)`; clean reclaimable cache: `OfferVirtualMemory` |
| GPU upload path | NVIDIA: cuFile (`cuFileDriverOpen` → `cuFileHandleRegister(O_DIRECT fd)` → `cuFileBufRegister` staging → `cuFileRead`/batch), compat mode auto-fallback; else pinned host staging + `cuMemcpyHtoDAsync`. AMD: pinned staging + hipMemcpyAsync | Unified memory: `newBufferWithBytesNoCopy` over **resident, pinned, 16 KiB-aligned** ranges; `MTLIOCommandQueue` + `MTLIOFileHandle` (lz4/zlib/lzfse) for direct file→`MTLBuffer`/`MTLTexture` streams | DirectStorage (`DStorageGetFactory` → `IDStorageQueue` → `EnqueueRequest(s)` + fence), GDeflate (1.1+) / Zstd (1.4) GPU decompression, ~64 KiB requests; fallback pinned upload heap + `CopyBufferRegion`. CUDA apps: cudarc + pinned staging (no GDS on Windows) |
| Page size / alignment | 4 KiB pages (2 MiB via hugetlbfs optional); `O_DIRECT` per `statx(STATX_DIOALIGN)`, use 4096 | **16 KiB** on Apple Silicon (`vm_page_size`), 4 KiB Intel; I/O 4 KiB-aligned; Metal NoCopy needs page (16 KiB) alignment | 4 KiB pages, 64 KiB allocation granularity; transparent chunk = 64 KiB; unbuffered I/O aligned to physical sector (use 4096) |
| Known limitations | Kernel-mode faults on the range need privileges (`CAP_SYS_PTRACE`, sysctl, or `/dev/userfaultfd`); io_uring often blocked in containers; GPU cannot fault into pager (pin first); WP_ASYNC/MOVE/RWP need recent kernels → probe features | No kernel pager API; one handler thread round trip per fault (~µs); exceptions interfere with debuggers and crash reporters; mach ports break across `fork`; no async direct-I/O ring; GPU cannot fault into pager | Section+VAD cost per resident chunk (hence ≥64 KiB chunks); VEH re-entrancy and loader-lock hazards; `MEM_WRITE_WATCH` not usable on views; DirectStorage is read-only and D3D12-only; IoRing write needs 22H2+; GPU cannot fault into pager |

**Cross-platform rule:** GPUs never take our page faults. GPU-visible data
uses the explicit API (`pin`/`prefetch`/`stream`) or a UE-style feedback
buffer + page table for GPU-side "transparent" access. CPU-side transparent
mode is a convenience layer over the same page cache and I/O engine.
