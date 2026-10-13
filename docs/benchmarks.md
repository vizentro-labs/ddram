# Benchmarks

All numbers are from `bench/run.sh 8G 2G` and `ddram llm`. The raw output is
in [`bench/results-linux-virtio-8G-2G.txt`](../bench/results-linux-virtio-8G-2G.txt).

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
