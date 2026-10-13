# ddram

**Extend DRAM with NVMe/SSD capacity on Linux, macOS and Windows.**

`ddram` is a userspace tiered-memory library written in Rust, with a C API.
It gives a process a memory region larger than the RAM it may use. Hot
blocks stay in a fixed RAM budget, cold blocks live on an SSD, and data moves
between them on demand, ahead of time (readahead), or when the application
asks (pin, prefetch, advise). It targets the large datasets behind GPU
workloads: LLM weights and KV caches, game assets and textures, upscaler and
diffusion models, training data.

## Highlights

* **Transparent and explicit modes.** Hand existing code a plain pointer
  (faults are served from the SSD), or use `pin`/`read_at`/`write_at`/
  `prefetch`/`advise` when the access pattern is known.
* **Hard RAM bound.** Data is held only in a fixed frame arena; direct I/O
  (`O_DIRECT` / `F_NOCACHE` / `FILE_FLAG_NO_BUFFERING`) bypasses the OS page
  cache.
* **Scan-resistant policy.** Repeated sweeps over more data than fits in RAM
  (every LLM decode step reads every layer) keep a stable resident subset
  instead of thrashing as LRU does. See the benchmarks below.
* **Coalesced readahead.** Detected sequential streams are read ahead as
  vectored I/Os of up to 2 MiB with several requests in flight.
* **NAND-aware writes.** Only dirty blocks are written back. An optional
  write governor caps write-back bandwidth to a share of the drive's
  remaining endurance budget, computed from the NVMe SMART/Health log.
* **Health-driven limits.** `Region::refresh_health` reads the drive's health
  log and sets the write cap, thermal back-off and unsafe-drive clamp.
* **SMART/health on every OS**, without smartctl: Linux admin passthrough,
  Windows `IOCTL_STORAGE_QUERY_PROPERTY`, macOS IOKit NVMe SMART plug-in.
* **GPU-ready.** The RAM arena is one fixed range that can be registered once
  with CUDA, cuFile, Metal or DirectStorage, so pinned blocks upload by DMA.
  See [docs/gpu-integration.md](docs/gpu-integration.md).

## Quick start (Rust)

```rust
use ddram::{Advice, Config, Region};

// 64 GiB region in 8 GiB of RAM, backed by a temp file on the NVMe drive.
let region = Region::new(Config::new(64 << 30, 8 << 30, "/mnt/nvme"))?;

// Transparent: an ordinary pointer.
let words = unsafe { std::slice::from_raw_parts_mut(region.as_ptr() as *mut f32, region.len() / 4) };
words[123_456_789] = 1.0;

// Explicit: tell ddram what is coming.
region.advise(0, region.len(), Advice::Cyclic);     // repeated sweeps (LLM weights)
region.prefetch(next_layer_off, layer_len);         // overlap I/O with compute
let pin = region.pin(block);                         // resident until dropped
upload_to_gpu(region.arena().0, pin.arena_offset()); // DMA from the pinned arena
```

Map an existing file (model weights, asset pack) read-only:

```rust
let weights = Region::new(Config::read_only_file("model.safetensors", 6 << 30))?;
```

## Quick start (C / C++)

```c
#include "ddram.h"
ddram_options o = {0};
o.size = 64ull << 30;
o.ram_budget = 8ull << 30;
ddram_region *r = ddram_region_create("/mnt/nvme", &o);
float *w = (float *)ddram_region_ptr(r);
/* ... */
ddram_region_destroy(r);
```

See [examples/c](examples/c). Build the static or shared library with
`cargo build --release -p ddram`.

## CLI

```text
ddram smart  /mnt/nvme                 # NVMe health of the drive behind a path
ddram budget /mnt/nvme --share 0.25    # endurance budget -> governor rate
ddram bench  --engine ddram|mmap|swap --workload cyclic|random|zipf|zipf-write \
             --size 8G --budget 2G --dir /mnt/nvme --cgroup
```

## Benchmarks

8 GiB of data in a **2 GiB RAM budget** (4× oversubscribed). ddram is compared
with the kernel's own tiering, file `mmap` through the page cache and
anonymous memory with swap, under the **same cgroup memory limit**, with
every byte verified. The machine is a 4-vCPU VM with a virtio SSD; see
[docs/benchmarks.md](docs/benchmarks.md) for the method and the caveats.

| Workload | ddram | kernel mmap | kernel swap* |
|---|---:|---:|---:|
| Repeated sweeps, 4 threads (LLM-style weight streaming) | **5.57 GB/s** | 1.81 GB/s | 0.31 GB/s |
| Synthetic LLM decode, 8.6 GB weights (real compute, identical output) | **0.48 tok/s** | 0.15 tok/s | — |
| Random 4 KiB reads, 4 threads | 33.6k ops/s | 13.4k ops/s | **47.7k ops/s** |
| Zipf (hot-set) reads, 4 threads | 99.8k ops/s | 38.1k ops/s | **151k ops/s** |
| Zipf writes, 4 threads (and bytes written to NAND) | 74.6k ops/s (2.2 GB) | 93.1k ops/s (4.6 GB) | **175k ops/s (0.8 GB)** |

\* swap was measured in an earlier session on the same machine; see the report.

Summary: ddram is 3.1–3.3× faster than mmap on streaming and LLM-style
workloads and about 2.5× faster on random reads, and it writes half as much
to NAND. Kernel swap is still faster on fine-grained random and hot-set
workloads, and mmap on hot-set writes.
Closing those gaps is on the [roadmap](docs/roadmap.md) (Linux `userfaultfd` engine).

## Platform support

| | Linux | macOS | Windows |
|---|---|---|---|
| Fault mechanism | SIGSEGV + `MAP_FIXED` frame mapping | SIGBUS/SIGSEGV + `MAP_FIXED` | vectored exception handler + placeholders (`MapViewOfFile3`) |
| RAM arena | `memfd` | unlinked POSIX shm | pagefile-backed section |
| Direct I/O | `O_DIRECT` (falls back to buffered on tmpfs) | `F_NOCACHE` | `FILE_FLAG_NO_BUFFERING` |
| Min block | 4 KiB | 16 KiB (Apple Silicon) | 64 KiB |
| SMART reader | admin ioctl (root) | IOKit NVMe SMART plug-in | `IOCTL_STORAGE_QUERY_PROPERTY` |
| Tested | x86_64 locally and in CI, arm64 in CI | CI (Apple Silicon) | CI |

## Project status

v0.1, experimental. The engine, policies, governor and SMART tooling are
implemented and tested on CPU workloads. GPU paths are designed and exposed
but **not yet validated on GPU hardware**. See the roadmap in
[docs/roadmap.md](docs/roadmap.md).

## License

Copyright 2026 Vizentro Labs. Licensed under the Apache License 2.0 (see
[LICENSE](LICENSE) and [NOTICE](NOTICE)).

Contributions are welcome under the [Contributor License Agreement](CLA.md);
see [CONTRIBUTING.md](CONTRIBUTING.md) and the
[Code of Conduct](CODE_OF_CONDUCT.md). Licensing notes are in
[docs/licensing.md](docs/licensing.md).
