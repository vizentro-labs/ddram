# GPU integration (design; not yet validated on GPU hardware)

The development environment for v0.1 had no GPU, so everything here is a
design built on the APIs that ddram already exposes and tests on the CPU.
GPU benchmarks are the top item on the roadmap; contributions are welcome.

## Constraints

* **A GPU cannot trigger CPU page faults.** On every OS, GPU access to host
  memory goes through DMA to pinned memory. The transparent pointer view is
  for CPU code (asset decoders, tokenizers, CPU-side KV management). GPU
  paths use the explicit API: `prefetch` → `pin` → upload → unpin.
* **Upload sources must be pinned or DMA-registered.** ddram's frame arena is
  a fixed virtual range for the whole life of the region, so it can be
  registered once.

## Common pattern

```text
region = Region::new(cfg)                      // weights/assets on SSD, RAM budget B
(arena, len) = region.arena()
register(arena, len)                           // once (API per platform below)

for each step (token / frame):
    region.prefetch(next_chunk)                // overlap SSD reads with compute
    pin = region.pin(block)                    // guaranteed resident, cannot be evicted
    gpu_copy_async(dst, arena + pin.arena_offset(), block_size, stream)
    on copy completion: drop(pin)
```

At steady state the data path is SSD → (DMA) → arena frame → (DMA) → VRAM.
The CPU never touches the bytes. This is the same structure as
NVIDIA GPUDirect Storage's compatibility mode and FlexGen/ZeRO-Infinity
offloading, but the staging pool is managed by a general pager with a
scan-resistant policy and endurance governor instead of by each framework.

## Per platform

### Linux / Windows + CUDA

* `cudaHostRegister(arena, len, cudaHostRegisterDefault)` makes the arena
  page-locked. `cudaMemcpyAsync(H2D)` from `arena + off` then runs at full
  PCIe bandwidth and overlaps with compute on a separate stream.
* Note: on Linux the arena is a memfd mapping. CUDA host registration of
  shared mappings is supported, but must be verified per driver version.
* **GPUDirect Storage** (`cuFile`): for read-only data, `cuFileRead` can DMA
  from the backing file straight into VRAM, skipping the arena. ddram's role
  is then the policy: which blocks to keep in VRAM or RAM, and when to
  prefetch. Planned API: `Region::plan(off, len) -> [(file_off, len)]`, which
  lists non-resident spans for the caller to fetch with cuFile.

### Windows + DirectStorage (gaming)

* DirectStorage reads files into GPU buffers, with optional GDeflate/Zstd
  GPU decompression, and is read-only. ddram can complement it in two ways:
  1. As a CPU-side cache tier for assets that are decoded on the CPU or
     reused across frames. The arena is the destination for
     `DSTORAGE_REQUEST_DESTINATION_MEMORY`.
  2. As the residency manager: an engine's texture streaming feedback (for
     example virtual-texture tile requests) maps to `advise(WillNeed/Hot/Cold)`.
* Write paths (save games, shader caches) go through ddram write-back under
  the endurance governor.

### macOS + Metal (Apple Silicon unified memory)

* RAM and VRAM are the same physical memory. Wrap the arena with
  `device.newBufferWithBytesNoCopy(arena, len, .storageModeShared, nil)`.
  This requires a page-aligned pointer and length, which the arena provides
  (16 KiB pages). Pinned blocks are then directly GPU-visible at
  `pin.arena_offset()`, with **no copy at all**.
* `MTLIOCommandQueue` (Metal 3 fast resource loading) is the
  DirectStorage-equivalent for read-only assets. As with cuFile, ddram can
  supply the policy.
* This is where ddram helps most: on a 16 GB Mac, a 40 GB model can run with
  the hot fraction resident and the rest streamed from the internal SSD,
  which does 5–7 GB/s on recent machines.

## What to expect

Rough steady-state throughput for weight streaming (decode, batch 1) is
bounded by:

```
tokens/s ≈ SSD_bandwidth / (model_bytes − resident_bytes)
```

For example, a 4 GB/s NVMe and a 40 GB model with 24 GB resident give
4 / 16 ≈ 0.25 tokens/s from the I/O bound alone. Under LRU, which re-reads
all 40 GB, the same setup manages 0.1 tokens/s, so the cyclic policy is worth
2.5× here. Sparse and MoE models, where only some experts are active per
token, gain much more because the per-token working set is small.
Batching amortizes each weight read over many tokens.

Gaming and texture streaming are latency-bound rather than bandwidth-bound.
Use 64–256 KiB blocks, explicit `prefetch` driven by the camera or feedback
buffer, and `Hot` advice for always-needed assets.
