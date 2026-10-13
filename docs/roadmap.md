# Roadmap

## v0.1 (this release)
- [x] Cross-platform pager: Linux, macOS, Windows (fault-driven, frame arena)
- [x] Explicit API: pin / read_at / write_at / prefetch / advise
- [x] CLOCK + scan-resistant cyclic policy, stream detection, coalesced readahead
- [x] Dirty-only write-back, endurance write governor
- [x] NVMe SMART/Health parser, per-OS readers, endurance budget
- [x] C ABI + header, CLI with benchmarks vs kernel mmap/swap
- [x] CI on Linux x86_64/arm64, macOS arm64, Windows
- [x] Background reclaimer (free-frame pool), coalesced write-back
- [x] Health-driven limits: `Region::apply_health` (endurance share, thermal back-off, unsafe-drive clamp)

## v0.2
- [x] Plan-driven paging: `Plan` / `Region::set_plan`, Belady-optimal eviction
      and exact prefetch along a declared access order (Rust + C API)
- [x] `ddram llm` mixture-of-experts workload, `ddram bench --workload chunks`
- [x] Learned plans: `Region::record` / `recorded_plan`, `Plan::save`/`load`
- [x] `LD_PRELOAD` shim: unmodified llama.cpp (and any mmap-based loader)
      served from a region, learning and persisting its plan
- [ ] Smooth recorded plans (sort within a window) so multi-threaded traces
      yield the tensor order rather than the thread interleaving
- [ ] Plans in the llama.cpp loader proper (declared per layer) and a
      PyTorch tensor wrapper

## Next
- [ ] **GPU validation**: CUDA (`cudaHostRegister` on the arena), Metal
      (`newBufferWithBytesNoCopy`), DirectStorage destination; publish numbers
- [ ] Linux userfaultfd engine (4 KiB granularity without per-block VMAs;
      `UFFDIO_MOVE`, `WP_ASYNC` + `PAGEMAP_SCAN` on 6.7+)
- [ ] io_uring / Windows IoRing backends for deeper queues with fewer threads
- [ ] Access-frequency sampling (periodic re-protection) for better CLOCK
      decisions on mapped blocks
- [ ] Compressed RAM tier (LZ4) between DRAM and NAND, zswap-style
- [ ] Proactive cleaner: write back dirty blocks in idle time under the governor
- [ ] Event-driven wakeups for I/O and reclaim threads (currently adaptive polling, 10 µs–1 ms)
- [ ] Read WCTEMP/CCTEMP from Identify Controller for thermal back-off before the warning bit trips
- [ ] OCP C0h extended SMART (physical media units written → true WAF)
- [ ] Python bindings (NumPy / PyTorch tensors backed by a Region)
- [ ] Mach exception port handler on macOS (debugger friendliness)
