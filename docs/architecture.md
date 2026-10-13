# ddram architecture

ddram extends a process's memory with SSD capacity. It is a userspace pager:
it gives the application a large region, keeps the hot part in a fixed RAM
budget, and moves blocks between RAM and an SSD-backed file.

It does **not** turn NAND into DRAM. NAND reads take 50–100 µs per 4 KiB page
(DRAM: ~0.1 µs), NAND is written in pages and erased in large blocks, and
each cell survives a limited number of program/erase cycles. DRAM and NAND
are both made with lithography tools (including ASML's), but they use
different cell technology (a capacitor versus a charge trap), so one cannot
be repurposed as the other. What software can do is hide the latency gap for
workloads with locality or predictable access, which covers most large GPU
and AI datasets. See `research/ssd-requirements.md` §4.

## Components

```
            application threads / GPU upload code
                 │ pointer access          │ pin / read_at / prefetch / advise
                 ▼                         ▼
   ┌─────────────────────────────────────────────────────────┐
   │ Region (lib.rs)                                          │
   │   transparent view: reserved VA range, one slot/block    │
   │   explicit API: Pin / PinMut guards, Advice, Stats       │
   ├─────────────────────────────────────────────────────────┤
   │ Pager (pager.rs)                                         │
   │   per-block AtomicU64 state  ─ EMPTY/LOADING/RESIDENT/   │
   │                                EVICTING/BUSY + dirty,     │
   │                                mapped, frame#             │
   │   frame table + CLOCK hand (spinlock, never held over I/O)│
   │   cyclic (scan-resistant) policy, sticky frames           │
   │   stream detector → readahead queue → I/O workers         │
   ├──────────────┬──────────────────┬───────────────────────┤
   │ sys (OS)     │ Backing (file)   │ WriteGovernor          │
   │ arena, map,  │ O_DIRECT /       │ token bucket on        │
   │ unmap, fault │ F_NOCACHE /      │ write-back bytes,      │
   │ handler      │ NO_BUFFERING     │ fed by SMART budget    │
   └──────────────┴──────────────────┴───────────────────────┘
```

### Arena and mapping

RAM is a single shared-memory object of `ram_budget` bytes divided into
frames of `block_size`. It's created with `memfd_create` on Linux, an
unlinked `shm_open` object on macOS, and a pagefile-backed section on Windows.
The arena is mapped once read/write (the "backdoor" view used for I/O and
pins). A block becomes visible in the application view by mapping *its frame*
over the block's address:

| OS | make resident | evict | fault delivery |
|---|---|---|---|
| Linux | `mmap(MAP_SHARED\|MAP_FIXED, memfd, frame_off)` | `mmap(PROT_NONE, MAP_ANON\|MAP_FIXED)` | SIGSEGV |
| macOS | same with the shm fd | same | SIGBUS (PROT_NONE) / SIGSEGV |
| Windows | `MapViewOfFile3(MEM_REPLACE_PLACEHOLDER)` | `UnmapViewOfFile2(MEM_PRESERVE_PLACEHOLDER)` | vectored handler, `EXCEPTION_ACCESS_VIOLATION` |

The same design works on all three systems. Several properties follow from
it:

* **The RAM bound is hard.** Only arena frames hold data. O_DIRECT-style I/O
  keeps the OS page cache from holding a second copy.
* **Blocks fill atomically.** Data is read into the frame through the
  backdoor view before the frame is mapped, so no thread can observe a
  half-filled block. This avoids the race in naive
  `mprotect(RW) → read() → mprotect(R)` pagers.
* **Fills are zero-copy.** The SSD DMAs straight into the frame that the
  application then reads (no `UFFDIO_COPY`-style memcpy).
* **GPU registration is one-time.** The arena is a fixed address range, so
  it can be registered once as pinned or DMA-able memory (see
  `gpu-integration.md`).

### Block states

```
EMPTY ──fault/pin/prefetch (CAS)──▶ LOADING ──read or zero-fill──▶ RESIDENT
  ▲                                                              │  │
  └──────── write back if dirty ◀── EVICTING ◀──CLOCK victim─────┘  │
                                                                     │
                       BUSY (remap or mprotect in progress) ◀────────┘
```

A RESIDENT block is *mapped* (visible in the application view) or *primed*
(loaded by readahead or the explicit API but not yet mapped). The first touch
of a primed block takes a soft fault, which maps it. These soft faults give
the pager exact access order without hardware accessed bits, and that order
drives the stream detector. Clean blocks are mapped read-only. The first
write takes a fault that sets the dirty bit and upgrades the mapping. Only
dirty blocks are ever written back.

Pins and eviction race safely. The evictor CASes the block to EVICTING and
then reads the pin count. A pinner increments the pin count and then
re-reads the state. Because both use SeqCst, at least one of them sees the
other.

### Fault handler constraints

On Unix, the fault path runs inside a signal handler. It therefore never
allocates and never takes a mutex. It only uses atomics, a spinlock that is
never held across I/O, `pread`/`pwrite`, `mmap`/`mprotect`, and
`nanosleep`/`sched_yield` for backoff. Faults outside every region are
chained to the previously installed handler or the default action, so normal
crashes still crash.

Known limitation (shared by every mprotect- or userfaultfd-based pager
without kernel privileges): a system call that *writes into* a non-resident
block, such as `read(fd, region_ptr, n)`, gets `EFAULT` instead of a page
fault. Use `Region::write_at`/`pin_mut`, or touch the memory first. GPUs
cannot trigger CPU page faults either, so GPU paths use pin/prefetch.

## Replacement policy

* **CLOCK** (second chance) over frames. Faults, pins and readahead set the
  referenced bit.
* **Cyclic / scan-resistant.** A sequential run longer than the RAM budget
  (or `Advice::Cyclic`) switches the region to a policy that marks frames
  *sticky* until `nframes - window` are held, then streams the rest through
  the small window. For repeated sweeps of `M` bytes in a budget of `B`
  bytes, LRU and CLOCK re-read all `M` every pass. The cyclic policy re-reads
  only `M - B + window`. This is the dominant pattern for LLM decoding
  (every token reads every layer's weights) and for full-scene asset passes.
* **Hot / Cold advice** pins frames as sticky or evicts them on request,
  for example to keep embeddings, KV-cache heads or UI textures resident.

## Readahead and I/O

A stream detector with 8 slots follows forward sequential runs per region.
When a run continues, the next `prefetch_depth` blocks are queued. I/O
worker threads, each keeping one request in flight, claim runs of adjacent
absent blocks and read them with one vectored request (`preadv`, up to
2 MiB). This lets small blocks, which are precise for random access, still
stream at full SSD bandwidth.

## NAND endurance

Write-back is the only thing ddram does that wears the SSD. Three
mechanisms keep it low:

1. Dirty tracking at block granularity: read-mostly data (model weights,
   assets) is **never** written back.
2. When the governor is constrained, CLOCK prefers clean victims on its
   first sweep.
3. Write-back of sequential streams is coalesced: a dirty victim is evicted
   together with the following dirty, readahead-loaded blocks in one
   vectored write (`pwritev`).
4. `WriteGovernor` is a GCRA token bucket on write-back bytes. The rate can
   be derived from the drive's health log (`ddram budget`): remaining
   endurance ÷ remaining service days × the share allotted to ddram. The
   drive's own `Percentage Used` is trusted over host bytes because it
   includes write amplification. A drive reporting critical-warning bits 0,
   2, 3 or 4 is flagged unsafe for tiering.
5. `Region::apply_health` / `refresh_health` close the loop. They set the
   governor from the budget (`HealthPolicy::write_share`), halve readahead
   on a temperature warning, and clamp write-back to 1 MiB/s on a drive
   with critical warnings. All-zero health pages, which some drives
   occasionally return, are rejected as invalid reads.

## Background threads

* `ddram-io-N`: readahead workers. Each claims runs of adjacent absent
  blocks and issues one vectored read.
* `ddram-reclaim`: kswapd-style reclaimer. It keeps a free-frame pool
  between low and high watermarks (about 1.5–3% of frames), so faults
  usually skip eviction. A fault only evicts directly when the pool is
  empty.

## Choosing a block size

| Workload | Block | Why |
|---|---|---|
| LLM weights, sequential sweeps | 1 MiB | fewer faults; coalescing makes 2 MiB I/Os |
| Mixed / unknown | 256 KiB | balanced |
| Random 4 KiB access (hash tables, KV lookups) | 64 KiB (min on Windows) | read amplification ≤ 16× |

Each mapped block can occupy a separate kernel VMA. On Linux, keep
`ram_budget / block_size` comfortably below `vm.max_map_count` (65530 by
default), or raise it.
