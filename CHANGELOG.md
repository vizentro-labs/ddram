# Changelog

## v0.2.0

* Plan-driven paging: `Plan` / `Region::set_plan` declare the access order
  of a region; the pager prefetches along it and evicts by Belady's optimal
  rule. Learned plans: `Region::record` / `recorded_plan`, `Plan::save` /
  `Plan::load`. C API: `ddram_plan_*`, `ddram_set_plan`, `ddram_clear_plan`,
  `ddram_record`, `ddram_recorded_plan`; plan counters in `ddram_stats`.
* `ddram llm`: `--plan` and a mixture-of-experts workload; `ddram bench`:
  `--workload chunks` and `--plan`.
* `libddram_shim.so` (Linux): `LD_PRELOAD` shim that serves an unmodified
  program's large read-only mappings (llama.cpp weights) from a region and
  learns the plan on the first pass.
* Benchmarks against ddram v0.1, kernel mmap, kernel swap and llama.cpp's
  own mmap loading (`bench/plan.sh`, `bench/llama/run.sh`), and the
  differentiation decision record.

## v0.1.0

Initial release: cross-platform userspace pager (Linux, macOS, Windows),
explicit API, CLOCK and cyclic policies, coalesced readahead, dirty-only
write-back with an endurance governor, NVMe SMART readers, C API and CLI.
