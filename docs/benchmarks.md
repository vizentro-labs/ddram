# Benchmarks

All numbers are from `bench/run.sh 8G 2G`, `bench/plan.sh 8G 2G` and
`ddram llm`. The raw output is in
[`bench/results-linux-virtio-8G-2G.txt`](../bench/results-linux-virtio-8G-2G.txt)
and [`bench/results-plan-linux-virtio-8G-2G.txt`](../bench/results-plan-linux-virtio-8G-2G.txt).

## Plan-driven paging (v0.2)

`Region::set_plan` tells the pager the order in which the region will be
touched; it then prefetches exactly that and evicts by Belady's optimal rule
(see [architecture.md](architecture.md#access-plans)). Same machine and
method as below: 8 GiB of data in a 2 GiB budget, every engine under the
same cgroup limit, every value verified (`errors=0`), engines interleaved
(A, B, C, A, B, C) so drift in the host-backed disk hits all of them alike.
Both rounds are shown; the spread between them is the noise floor.

**Chunks**: 4 MiB chunks read in a fixed pseudo-random order, swept 3 times
by 4 threads. This is the shape of asset streaming along a known path or of
experts chosen by a router: perfectly predictable, not sequential. No stream
detector can prefetch it and no recency policy keeps the right subset.

| Engine | GB/s (round 1 / 2) | Disk read per 8.6 GB pass | vs ddram v0.1 |
|---|---:|---:|---:|
| **ddram, plan** | **5.52 / 5.42** | **6.5 GB** | **3.7×** |
| ddram v0.1 (auto policy, readahead) | 1.43 / 1.49 | 23.4 GB | 1× |
| kernel mmap | 1.57 / 1.48 | 8.5 GB | 1.1× |
| kernel swap | 0.19 / 0.19 | 9.6 GB (+8.4 GB written) | 0.13× |

The plan reads each chunk once per pass and keeps a stable subset resident
(6.5 GB per 8.6 GB pass is the Belady minimum for this budget). v0.1
ddram's stream detector reads ahead past every chunk boundary and its
automatic policy flips between cyclic and CLOCK, so it reads 2.7× the data
of a pass; the kernel reads the whole pass every time. Swap writes every
page back on every pass although nothing changed.

**Dense decode** (`ddram llm`, 8.6 GB of f32 weights, 32 layers, every
token reads every layer, 4 compute threads, 4 tokens). The cyclic policy
was built for exactly this pattern, so parity is the expectation.

| Engine | steady-state tokens/s (round 1 / 2) | Disk read for 4 tokens |
|---|---:|---:|
| **ddram, plan** (`--plan`, one cyclic plan over the layers) | **0.66 / 0.73** | 28.4 GB |
| ddram v0.1 (cyclic policy + `WillNeed` one layer ahead) | 0.62 / 0.64 | 29.5–31.1 GB |
| kernel mmap (`MADV_SEQUENTIAL` + `MADV_WILLNEED` one layer ahead) | 0.14 / 0.15 | 34.4 GB |

Identical checksum (2.377314) on every engine. The plan is 7–14% faster
than the hand-tuned cyclic policy and reads 4–9% less, because it needs no
streaming window beyond its look-ahead; both are 4–5× faster than the
kernel.

**Mixture-of-experts decode** (`--experts 8 --active 2`: 32 layers × 8
experts of 32 MiB, 2 experts per layer per token chosen by a deterministic
router with 1/rank popularity; each token touches 2.1 GB, the budget). The
plan is declared per token when the token starts, which is what a
pre-gating router provides; ddram v0.1 and the kernel get `WillNeed` /
`MADV_WILLNEED` hints for the next layer's experts.

| Engine | steady-state tokens/s (round 1 / 2) | Disk read for 4 tokens | Hard faults |
|---|---:|---:|---:|
| **ddram, plan** | 1.42 / 1.53 | **6.3 GB** | 576–609 |
| ddram v0.1 (hints) | 1.66 / 1.77 | 12.1 GB | 2815 |
| kernel mmap (hints) | 0.80 / 0.86 | 7.1 GB | — |

Identical checksum (1.838918) on every engine. Here the honest result is
mixed. The plan reads **half** the data of v0.1 (readahead overshoots every
expert boundary into experts that are not used) and never guesses, but on
this VM it is 10–15% slower. The per-token traces show why: the plan run is
bound by the raw disk rate (1.7 GB in 0.73–0.83 s ≈ 2.1–2.4 GB/s, the
device's sequential limit), while the v0.1 run's redundant reads of the
same adjacent experts are served from the host's cache of the virtual disk
at 4–5 GB/s. On a real NVMe drive without a host cache, half the I/O is
half the time. With more I/O workers the plan already leads on this VM
(16 workers: plan 1.67 tokens/s vs hints 1.46, same budget), because its
reads are issued by the workers rather than by faulting compute threads.
Both ddram modes are 1.7–2.1× faster than the kernel.

**Learned plans** are measured with unmodified llama.cpp in
[llama-cpp.md](llama-cpp.md); the results are in the section below.

### llama.cpp, unmodified, through the shim

LLAMA_RESULTS_PLACEHOLDER

## v0.1 results

## Setup

* **Machine**: cloud VM with 4 vCPUs, 15 GB RAM, Linux 6.18, ext4 on a
  virtio block device. Raw device numbers (O_DIRECT, measured with `dd`):
  2.4 GB/s sequential read, and 16 KiB random reads at 21.5k IOPS (QD1),
  53.5k (QD4) and 73k (QD8).
  The virtual disk is backed by host storage that may cache, so absolute
  numbers vary between runs (sweeps: ±20%) and will differ on bare-metal NVMe.
  The comparisons are fair because all engines run on the same device, back to
  back, from dropped caches.
* **Workload size**: an 8 GiB region with a **2 GiB RAM budget** (4×
  oversubscription).
* **Same memory limit for every engine**: each run executes in a cgroup whose
  memory limit is the budget plus 128 MiB, so the kernel's page cache and swap
  are held to the same RAM ddram enforces internally.
* **Engines**:
  * `ddram`: this library. 1 MiB blocks for sweeps, 16 KiB blocks for random
    workloads (`vm.max_map_count` raised to 1M, as Elasticsearch requires).
  * `mmap`: a `MAP_SHARED` file mapping through the kernel page cache.
  * `swap`: anonymous memory with a swap file on the same disk.
* **Method**: each run fills the region sequentially with a known pattern,
  writes back what is dirty (untimed, on every engine), then runs the measured
  phase. Every value read is **verified**: all runs reported `errors=0`.
  Disk traffic comes from `/proc/diskstats`, so it includes kernel writeback
  and swap I/O.

## Results

The ddram and mmap columns come from the final build in one session. The
random workloads are medians of two runs interleaved ddram/mmap/ddram/mmap,
to cancel drift in the host-backed disk. The swap column is from an earlier
run on the same machine (the swap file was removed afterwards to stay within
the disk allowance). In that earlier session ddram and mmap measured 10–30%
higher on random workloads, so compare swap with some care.

| Workload | ddram | kernel mmap | kernel swap* | ddram vs mmap |
|---|---:|---:|---:|---:|
| **Repeated sweeps**, 1 thread (GB/s) | **4.44** | 2.15 | 0.33 | **2.1×** |
| **Repeated sweeps**, 4 threads (GB/s) | **5.57** | 1.81 | 0.31 | **3.1×** |
| Disk read per sweep (GB) | **6.65** | 8.59 | 8.6 (+8.2 written) | −23% |
| **Random 4 KiB reads**, 4 threads (ops/s) | 33.6k | 13.4k | 47.7k | **2.5×** |
| **Zipf θ=0.99 reads**, 4 threads (ops/s) | 99.8k | 38.1k | 151k | **2.6×** |
| **Zipf θ=0.99 writes**, 4 threads (ops/s) | 74.6k | 93.1k | 175k | 0.8× |
| Disk written in the Zipf-write phase (GB) | 2.15 | 4.57 | 0.79 | **−53%** |
| Sequential fill, 1 MiB blocks (s, 8 GiB) | **3.7–3.9** | 9.2–11.3 | 11.7–15.5 | **2.5–3×** |
| Sequential fill, 16 KiB blocks (s, 8 GiB) | 15.4–16.2 | **8.5–8.8** | 15.9 | 0.55× |

\* earlier session, see above.

### LLM decode (`ddram llm`)

A synthetic transformer decode loop with real compute: 32 layers, hidden size
4096, 8.6 GB of f32 weights, and a 4-thread matrix-vector product per layer,
so every token reads every weight. The weight file is mapped read-only.
ddram uses the cyclic policy and per-layer `WillNeed` hints. Kernel mmap gets
the equivalent `MADV_SEQUENTIAL` + `MADV_WILLNEED`, which is what llama.cpp
does by default. Both run under the same 2 GiB limit, for 4 tokens.

| | tokens/s | steady-state s/token | checksum |
|---|---:|---:|---|
| **ddram** | **0.482** | **1.69–1.77** | 2.377314 |
| kernel mmap | 0.147 | 6.63–6.92 | 2.377314 |

**3.3× faster overall and about 3.9× in steady state, with bit-identical
outputs.**

## Interpretation

* **Sweeps larger than RAM** are the core LLM and asset-streaming pattern,
  and the main win. LRU-style kernel caches re-read the entire model every
  pass. ddram's scan-resistant policy keeps about 1.9 GiB resident and
  streams only the rest, with coalesced readahead keeping several 2 MiB
  requests in flight. Swap is worst here: it writes every page back to the
  swap device on every pass, even though the data never changed.
* **Random and skewed reads**: ddram is about 2.5× faster than mmap. One reason is
  that the kernel's 128 KiB read-around on mmap faults pollutes the cache;
  mmap read 32 GB for the Zipf run, ddram 4 GB. On uniform random reads
  ddram matches swap. On skewed (Zipf) workloads swap is still ahead by
  1.2–1.8×: in-kernel 4 KiB paging with batched TLB flushes is hard to beat
  for fine-grained hot sets, and ddram's per-fault cost (signal + `mmap`) is
  higher. A Linux `userfaultfd` engine is on the roadmap for this case.
* **Writes**: ddram writes back only dirty blocks. In the Zipf-write phase it
  wrote 53% less than mmap, which means less NAND wear, but ran 20% slower.
  Each first write to a clean block costs a signal plus an `mprotect`, while
  the kernel tracks writes without leaving kernel mode. Sequential writes
  with small blocks are limited by per-block fault cost (about 30 µs per
  block in this VM). Coalesced write-back already took the 16 KiB fill from
  22 s to 16 s, but for write-heavy streams use 1 MiB blocks, where ddram is
  2.5–3× faster than mmap.

## Findings that changed the design

These came from profiling during development. They are listed because they
apply to anyone building a userspace pager.

1. **Readahead must never evict itself.** Blocks fetched by readahead are now
   inserted as *referenced*, coalesced runs are capped at the readahead
   depth, and explicit `prefetch` hints are capped at half the streaming
   window. Before these fixes, 4-thread sweeps read 15 GB per pass instead of
   6.6, and the LLM demo read 11.8 GB per 8.6 GB token.
2. **A one-off sequential scan must not switch on the cyclic policy for
   good.** Random accesses now switch it off again. Before this fix, random
   workloads that followed a sequential load ran with about 64 usable frames
   instead of 2048.
3. **Sparse backing files serialize I/O on ext4.** A direct write into a hole
   takes the inode lock exclusively and blocks concurrent direct reads. The
   backing file is now preallocated, which took random reads during
   write-back from 10k to 22k ops/s. In the steady state (clean) they reach
   46k.
4. **Eviction belongs off the fault path.** A background reclaimer keeps a
   free-frame pool so faults usually skip unmapping (TLB shootdown) and
   write-back.

## Reproducing

```sh
cargo build --release -p ddram-cli
sudo swapon /path/to/swapfile          # for the swap engine
sudo bench/run.sh 8G 2G /mnt/nvme/bench
sudo target/release/ddram llm --engine ddram --model-size 8G --budget 2G --dir /mnt/nvme/bench --cgroup
sudo target/release/ddram llm --engine mmap  --model-size 8G --budget 2G --dir /mnt/nvme/bench --cgroup
```

Please share results from real NVMe drives and GPUs (see CONTRIBUTING.md).
