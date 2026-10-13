# Running llama.cpp (unmodified) on ddram

`libddram_shim.so` is an `LD_PRELOAD` library that serves a program's large
read-only file mappings from a ddram region. llama.cpp loads GGUF weights
with `mmap` by default, so it runs on ddram without any change:

```sh
cargo build --release -p ddram-shim
DDRAM_BUDGET=2G LD_PRELOAD=target/release/libddram_shim.so \
    llama-completion -m model.gguf -p "..." -n 64
```

What happens:

1. llama.cpp calls `mmap(NULL, size, PROT_READ, MAP_SHARED, fd, 0)` on the
   model file. The shim opens the same file as a read-only ddram `Region`
   with `DDRAM_BUDGET` bytes of RAM and returns the region's pointer. The
   kernel never caches the weights (direct I/O), so the process stays within
   the budget plus its own working memory.
2. The shim records the order in which blocks are faulted in. A decode step
   reads every weight once, in the same order every token, so once every
   block has been seen the recorded order *is* the plan. The shim installs it
   as a cyclic `Plan` (exact prefetch, Belady eviction) and saves it as
   `<model>.ddram-plan`.
3. The next run loads the saved plan before the first token.

`madvise`/`posix_madvise` on the served mapping are accepted and ignored,
`munmap` of the whole mapping releases the region, `munmap` of a fragment
(llama.cpp unmaps tensors it has copied to a GPU) is ignored.

Environment variables are listed in `crates/ddram-shim/src/lib.rs`. The shim
is inactive unless `DDRAM_BUDGET` is set, and leaves mappings smaller than
`DDRAM_MIN_SIZE` (64 MiB) to the kernel. Linux only; macOS would need
`DYLD_INSERT_LIBRARIES` with interposition, Windows a detour, neither of
which is implemented.

## Benchmark

`bench/llama/run.sh MODEL.gguf [BUDGET] [SLACK]` runs llama.cpp under a
cgroup memory limit of `BUDGET + SLACK` four ways, interleaved and repeated:
kernel `mmap` (llama.cpp's default), ddram without a plan, ddram learning the
plan on the first pass, and ddram starting from the saved plan. The limit
applies to the page cache as well as to ddram's arena, so every variant has
the same RAM for the weights plus llama.cpp's own buffers.

No model could be downloaded on the benchmark machine (no access to Hugging
Face), so `bench/llama/make_model.py` builds a GGUF with TinyLlama-1.1B's
architecture and tokenizer and random weights; `llama-quantize` turns it into
Q4_K_M (636 MiB). The compute and the memory access pattern per token are
those of the real model; only the generated text is meaningless. The script
accepts any GGUF, so the numbers can be reproduced with a real model.

Results are in [benchmarks.md](benchmarks.md).
