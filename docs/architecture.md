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
   │   planned API: Plan (ranges, cyclic) → set_plan          │
   ├─────────────────────────────────────────────────────────┤
   │ Pager (pager.rs)                                         │
   │   per-block AtomicU64 state  ─ EMPTY/LOADING/RESIDENT/   │
   │                                EVICTING/BUSY + dirty,     │
   │                                mapped, frame#             │
   │   frame table + CLOCK hand (spinlock, never held over I/O)│
   │   cyclic (scan-resistant) policy, sticky frames           │
   │   stream detector → readahead queue → I/O workers         │
   │   plan (plan.rs): cursor, next-use table, Belady victims, │
   │                   prefetch frontier → the same queue       │
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
* **Planned (Belady).** With a [`Plan`](#access-plans) installed, the victim
  is the resident block whose next planned use is furthest away. This is
  Belady's optimal policy, which no policy that only looks at the past can
  match. Blocks the plan never touches again are evicted first.

## Access plans

Many large workloads know their future: a decoding language model reads
every layer in the same order for every token, a mixture-of-experts router
chooses the experts of the next layer before they are needed, a game streams
assets along a known path, an out-of-core solver sweeps its matrices in a
fixed order. `Region::set_plan` lets the application say so: a `Plan` is an
ordered list of byte ranges, optionally cyclic. From then on:

* **Position tracking costs O(1) per fault.** Every block carries the step of
  its next planned use (`next_use`). A fault on a block whose next use lies
  ahead of the current range moves the cursor there (forward only, and at
  most half a cycle for cyclic plans, so a thread that lags behind the
  frontier does not drag it back). Resident, mapped blocks fault no more
  than before, so a plan adds nothing to the hot path.
* **The prefetch frontier** is maintained by the reclaim thread: it walks
  the plan from the current range and queues absent blocks until
  `lookahead` blocks are in the window (absent, in flight, or loaded ahead
  and not yet touched). Blocks that are resident for other reasons cost
  nothing, so after a cyclic plan's stable prefix the frontier is already
  waiting at the first streamed block. The I/O workers coalesce queued
  blocks into vectored reads of up to 2 MiB, but only across blocks the
  plan needs soon, so nothing outside the plan is read. The heuristic
  stream detector and the automatic policy switch are off while a plan is
  installed; explicit `prefetch`/`advise` still work.
* **Retirement.** When the cursor leaves a range, its blocks are pointed at
  their next occurrence (or marked as never needed again for a linear
  plan). The current range is never evicted, so several threads may consume
  one range concurrently, and blocks loaded ahead but not yet touched are
  protected too.
* **Eviction** selects victims by the next-use distance. The background
  reclaimer takes a batch of victims per scan; a fault that must reclaim
  directly runs the same scan without allocating (it is `O(frames)`, so
  plans are meant for the block sizes recommended below, not for 4 KiB
  paging).
* **Replacement is safe under load.** The plan lives behind an atomic
  pointer with a user count; `set_plan` swaps the pointer and waits for
  in-flight faults before freeing the old plan.

Ranges should be the units the application consumes together (a layer, an
expert, an asset). Ranges larger than a quarter of the budget are split into
pieces of an eighth, so a single huge cyclic range behaves like the cyclic
policy. For a cyclic sweep larger than RAM the plan keeps a stable prefix
and streams the rest, like the cyclic policy; for anything non-sequential it
reads each block exactly once per use and never guesses.

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
  empty. With a plan installed it also runs the plan step (retire passed
  ranges, extend the prefetch frontier) on every iteration.

## Choosing a block size

| Workload | Block | Why |
|---|---|---|
| LLM weights, sequential sweeps | 1 MiB | fewer faults; coalescing makes 2 MiB I/Os |
| Mixed / unknown | 256 KiB | balanced |
| Random 4 KiB access (hash tables, KV lookups) | 64 KiB (min on Windows) | read amplification ≤ 16× |

Each mapped block can occupy a separate kernel VMA. On Linux, keep
`ram_budget / block_size` comfortably below `vm.max_map_count` (65530 by
default), or raise it.
