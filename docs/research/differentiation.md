# Differentiation: what makes ddram different from other SSD-as-memory products

Status: decision record, October 2026.

## 1. The field

| Product | What it is | How it decides what is in RAM |
|---|---|---|
| Phison aiDAPTIV+ | Flash as a memory tier for LLM fine-tuning and KV-cache offload; closed, Phison SSDs only, LLM only | Framework-level offload schedule inside its own middleware |
| NVIDIA BlueField-4 context memory, GPUDirect Storage, Dynamo KV offload | Rack-scale KV-cache and weight offload, NVIDIA GPUs and DPUs only | LRU-style KV block managers, framework hints |
| VMware Memory Tiering, Kove, MemVerge | Hypervisor or CXL/pooled memory tiers for the enterprise | Hardware access bits sampled by the hypervisor, LRU/CLOCK variants |
| Linux swap and file `mmap`, llama.cpp `mmap` loading | Generic kernel paging | LRU-approximating active/inactive lists, 128 KiB read-around, `MADV_WILLNEED` hints |
| DeepSpeed ZeRO-Infinity, FlexGen | Single-framework CPU/NVMe offload for training and batch inference | Static per-layer schedules hard-wired into the framework |

Every general tier in that list infers the future from the past: LRU, CLOCK,
second chance, readahead on detected streams. ddram v0.1 does the same, with
a scan-resistant policy that already beats kernel `mmap` 2.5-3.3x on streaming
and LLM-style workloads, and still loses to kernel swap on fine-grained
hot-set access (100k vs 151k ops/s).

## 2. Candidates evaluated

| # | Direction | Expected gain | Evidence | Effort | Risk | Verdict |
|---|---|---|---|---|---|---|
| 1 | **Plan-driven memory** (declared access plan, Belady-optimal eviction, exact just-in-time prefetch) | Large on any workload whose order is known but not sequential: MoE expert streaming, speculative decoding, asset streaming along a path, out-of-core solvers. Equal to the cyclic policy on dense sweeps (for a pure cycle, MRU is already optimal). Zero hard faults when the plan is right. | Belady (1966) is provably optimal; the cyclic policy's gains are a special case. Pre-gated MoE (Hwang et al., ISCA 2024), MoE-Infinity, EdgeMoE and ExpertFlow all show that expert choices are predictable one layer ahead. No general memory tier exposes this. | Medium: new pager policy and prefetch cursor, O(1) fault-path cost, C API | Medium: concurrency of plan replacement with in-flight faults; applications must declare ranges at a sensible granularity | **Chosen** |
| 2 | **Learned and persisted access profiles** (record a run, store the plan beside the file, warm-start the next run) | Removes the cold-start penalty and gives plan-driven behaviour to unmodified applications: the first pass of a decode loop teaches the pager the order of every later pass. | Same machinery as 1; a plan is the profile. Trace-driven prefetch is standard in storage research (e.g. the SEER/TIP family), but no SSD-tier product ships it for application memory. | Low once 1 exists: a bounded, allocation-free recorder in the fault path plus a file format | Low | **Chosen** (second capability; shares the plan API) |
| 3 | Three-tier memory with LZ4-compressed DRAM | Honest expectation: near zero on quantized or f16 weights (entropy-coded already), 1.5-2x on KV cache, more on some game assets. Hurts latency on incompressible data. | zswap/zram literature. Measured here with zlib level 1 on 256 MiB samples: a Q4_K_M GGUF compresses to 98.4% of its size, an F16 GGUF to 92.5%. | High: a second frame class, compression off the fault path, extra copies | Medium: easy to regress the common case | Deferred; revisit for KV-cache regions only |
| 4 | Multi-SSD striping | Linear bandwidth scaling with drive count on desktops with several M.2 slots | Standard RAID-0 argument | Medium | Cannot be validated here (one virtio disk); gain is not unique (md/LVM/Storage Spaces do it) | Deferred |
| 5 | FDP/ZNS endurance placement | Lower write amplification on supporting drives | NVMe 2.0 FDP, OCP 2.5 | Medium | No hardware here; only enterprise drives support it today | Deferred, not hardware-validatable |
| 6 | Linux `userfaultfd` engine | Closes the 1.5x gap to kernel swap on hot-set workloads | Documented in `platforms.md` | High: a second engine, page-granular state, kernel 6.x minor-fault path | High; catches up with swap rather than differentiating | Deferred (roadmap) |
| 7 | Cross-process sharing of one RAM tier | Saves N-1 copies of a model shared by N processes | memfd/shm arenas already exist | Medium | Lifetime and consistency across processes | Deferred |

## 3. Decision

Build **plan-driven memory** (1) and **learned, persisted plans** (2) behind
one API, and prove them on three workloads: a MoE-style decode loop, the
existing dense decode loop, and unmodified llama.cpp through an `LD_PRELOAD`
shim that learns the plan from the first pass.

Rationale:

* It is the one thing nobody in the table does as a general memory layer.
  Every competitor guesses from the past; ddram can be told, or learn, the
  future and then page optimally. Optimality is a property that can be
  proven, not just measured.
* It builds directly on the existing engine (frame arena, coalesced readahead,
  dirty-only write-back) rather than replacing it, so the fault path stays
  allocation-free and the risk of regressing v0.1 behaviour is low. Without a
  plan, nothing changes.
* It is measurable with named baselines on this VM: current ddram, kernel
  `mmap` with the equivalent `MADV_WILLNEED` hints, and llama.cpp's own
  `mmap` loading.
* It composes with the deferred items. Striping, FDP and a userfaultfd engine
  all benefit from knowing the future; a compressed tier would be guided by
  it (compress what is far away in the plan).

What it does **not** fix: the hot-set gap to kernel swap on unplannable
random access. That remains a `userfaultfd` job and is listed in the roadmap.

## 4. Design summary

* `Plan`: an ordered list of byte ranges, optionally cyclic. The region
  expands it to blocks when installed with `Region::set_plan`.
* **Cursor**: the plan position is advanced by faults in O(1) (each block
  stores the step of its next planned use; an access to a block whose next
  use is within the look-ahead window moves the cursor there). Concurrent
  consumers of the same range are fine; the current range is never evicted.
* **Prefetch**: a background step keeps the next `lookahead` bytes of the
  plan resident, issued in plan order and coalesced into up to 2 MiB I/Os.
  The heuristic stream detector is off while a plan is active, so nothing
  outside the plan is read speculatively.
* **Eviction**: Belady's rule. The victim is the resident block whose next
  planned use is furthest away; blocks that the plan will never touch again
  go first (CLOCK order among themselves). The background reclaimer selects
  victims in batches with one scan; a fault that must reclaim directly uses
  the same rule without allocating.
* **Recording**: `Region::record` captures block first-touch order into a
  preallocated buffer from the fault path; `recorded_plan` turns it into
  ranges. `Plan::save`/`Plan::load` persist it beside the backing file.
* **Shim**: `libddram_shim.so` interposes `mmap`/`munmap`/`madvise` for large
  read-only file mappings, serves them from a `Region`, records the first
  pass, and installs the learned cyclic plan (and reuses it on the next run).

## 5. Measurement plan

Interleaved repeated runs (engine A, B, A, B), medians reported, every byte
verified, same cgroup memory limit for every engine, 4x oversubscription.

| Workload | Baselines | Metric |
|---|---|---|
| MoE decode (`ddram llm --experts 8 --active 2`) | ddram auto + per-expert `WillNeed`; kernel `mmap` + `MADV_WILLNEED` per expert | tokens/s, GB read per token, hard faults |
| Dense decode (`ddram llm --plan`) | ddram cyclic; kernel `mmap` | tokens/s (expected: parity with cyclic) |
| Cold start (`--plan-file`) | ddram cyclic without a plan | time to first token |
| llama.cpp, unmodified, via the shim | llama.cpp default `mmap` under the same cgroup limit | decode tokens/s, prompt tokens/s |
