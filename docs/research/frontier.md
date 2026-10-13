# Frontier research: what would be a step change for ddram

Status: synthesis, October 2026. Method: one research question, five
parallel systematic searches (model-aware paging, GPU-initiated storage,
mixture-of-experts offload on consumer hardware, weight and KV encodings,
hardware interfaces and OS tiering), every source checked against its own
abstract text and, where code exists, its repository and licence, then a
cross-source synthesis with contradictions and gaps, and a devil's-advocate
pass. The search harness could not open arxiv.org, ACM or vendor sites
directly (egress policy), so "verified" below means: title, authors, year
and the quoted numbers were confirmed from the official page's text as
returned by search, and licences were read from the repositories themselves.
Numbers are quoted with their conditions; nothing is from memory.

## 1. Research question

For a userspace memory tier that extends RAM with NVMe (ddram v0.2:
plan-driven, block-granular, model-agnostic), which capability would be a
step change rather than an increment on consumer and edge hardware (one GPU,
32 to 64 GB of RAM, one or two NVMe drives), measured as tokens per second
per GB of RAM, bytes read per token, or model size runnable at usable speed?

Five hypotheses were tested against the literature.

| | Hypothesis | Verdict |
|---|---|---|
| H1 | Model-aware (semantic) paging: load neurons or experts, not blocks | Partly. The generic parts (co-access re-layout, utility per byte) transfer; the 10× sparsity numbers do not. |
| H2 | GPU-initiated paging with no CPU on the path | Not on consumer hardware. CPU-issued I/O into a GPU-readable pinned arena is the viable path; the gain over a bounce buffer is about 1.4×. |
| H3 | Frontier-size MoE from NVMe on a 32 to 64 GB PC at usable speed | No. Physics caps it near 0.5 to 2 tokens/s; usable speed from NVMe is for 30B to 120B MoEs. |
| H4 | Decode-on-load encodings | Marginal for 4-bit weights (about 0%), about 30% for BF16, large only for lossy 2-bit KV. One contradicting claim must be measured. |
| H5 | Push the plan to the device | Open question. No product lets a host declare future reads to an SSD and one write-side channel (FDP) exists today, but the read-side gain is bounded by what a deep host queue already achieves and must be measured against that baseline first. |

## 2. Evidence by hypothesis

### H1 Model-aware paging (12 sources)

* Sparsity that cuts bytes by 10× is a ReLU property obtained by retraining:
  OPT models >90% inactive neurons (Mirzadeh et al., ICLR 2024, arXiv
  2310.04564); ProSparse 89% on LLaMA2-7B after continued pretraining (arXiv
  2402.13516); TurboSparse about 3% of MoE-layer parameters active per token
  after dReLU retraining (arXiv 2406.05955). Training-free sparsity on
  SwiGLU models tops out near 50%: CATS (COLM 2024, arXiv 2404.08763, MIT)
  and TEAL (ICLR 2025, arXiv 2408.14690, MIT), i.e. at most 2× fewer bytes.
* Accuracy is not free: Sirius (NeurIPS 2024, arXiv 2409.03856) reports
  GSM8K on Llama-3-8B falling from 0.75 to 0.38 under coarse contextual
  sparsity, recovered to 0.71 only at an effective density of 0.7 to 0.8.
* The flash-native systems: LLM in a flash (ACL 2024, arXiv 2312.11514;
  4 to 5× CPU and 20 to 25× GPU over naive loading on OPT-6.7B, no code),
  PowerInfer (SOSP 2024, arXiv 2312.12456, MIT, ReLU models only),
  PowerInfer-2 (arXiv 2406.06282; TurboSparse-Mixtral-47B at 11.68 tokens/s
  on a OnePlus 12, no public code), Ripple/Neuralink (ASPLOS 2025, arXiv
  2410.19274; offline re-layout of co-activated neurons, 1.49× end to end,
  up to 4.32× bandwidth, no code), VLM in a flash (NeurIPS 2025, arXiv
  2511.18692; chunk selection by importance per read latency, 4.65 to 5.76×
  I/O efficiency on Jetson, Apache-2.0).
* Energy: streaming MoE experts from SSD costs 3.8 to 12.5× more energy per
  token than HBM (IEEE CAL 2025, arXiv 2508.06978); prefetch hides latency,
  not energy. For the battery-powered part of the target this is an adverse
  finding against flash tiering as such, not a different axis.
* Two negative results bound the idea: WiSP (arXiv 2606.21868) finds that
  prediction-driven prefetch helps little in single-stream decode once the
  link is saturated, and "Cacheable by Design?" (arXiv 2608.18261), a
  pre-registered study, failed to train locality into routers and reports
  Qwen3-235B at 0.44 tokens/s warm from disk.

What is expected to transfer to a model-agnostic pager: the offline
re-layout pass (Ripple, 1.49× end to end, shown only on ReLU-sparse models at
neuron granularity) needs only an observed co-access trace, which ddram
already records; the utility-per-byte selection rule (VLM in a flash) needs
only per-block read cost, which ddram can measure. Neither has been shown
model-agnostically; that is the experiment to run.

### H2 GPU-initiated storage (12 sources)

* GPU threads issuing NVMe commands (BaM, ASPLOS 2023, arXiv 2203.04910,
  BSD-2; GIDS, VLDB 2024; Tutti, arXiv 2605.03375, Apache-2.0) reach tens
  of millions of IOPS and 25 GB/s over four drives, but require datacenter
  GPUs with large BARs (BaM excludes the T4), custom NVMe kernel drivers,
  IOMMU off, and Linux.
* CPU-issued I/O with GPU DMA (GPUDirect Storage, DeepNVMe, FlexGen,
  ZeRO-Infinity, DirectStorage) is what consumer hardware can run.
  DeepSpeed's own measurement: 9.6 GB/s with GDS vs 7 GB/s without on four
  NVMe drives, about 1.4×. GDS needs the nvidia-fs module and Tesla or
  Quadro parts except on the kernel 6.2+ P2PDMA path.
* G10 (MICRO 2023, arXiv 2310.09443, Apache-2.0) is the closest academic
  analogue to plan-driven paging: a compiler-derived tensor-lifetime plan
  drives migration across HBM, DRAM and flash, reaching 90.3% of an
  unlimited-memory ideal, in simulation.
* DirectStorage (MIT) is the only shipping, vendor-neutral, consumer path
  where the GPU consumes SSD data (with GPU decompression), on Windows,
  read-only, CPU-issued.

What transfers: a "GPU-consumable" mode where the CPU prefetches along the
plan into the pinned arena and exposes a per-block ready fence the GPU
polls, with DirectStorage or cuFile-compatible back ends. It is an
integration, not a new mechanism.

### H3 Frontier-size MoE on a PC (12 sources plus llama.cpp practice)

Architecture facts from DeepSeek-V3's published configuration: 671B total,
37B active, 61 layers (3 dense), 256 routed experts, 8 active per token,
hidden size 7168, expert inner size 2048. Each routed expert is 44.0M
parameters, 22 MB at 4 bits; a token reads 464 experts, about 10.2 GB; the
expert pool is about 327 GB.

| RAM for expert cache | share of pool | tokens/s at 5 to 7 GB/s with uniform routing |
|---|---:|---:|
| 20 GB (32 GB PC) | 6% | 0.52 to 0.73 |
| 52 GB (64 GB PC) | 16% | 0.58 to 0.81 |

Five tokens/s needs 86 to 93% cache hits. Published systems agree with the
arithmetic: SSD-LLaMA (arXiv 2609.18110, Sep 2026) reports about 1 token/s
for a trillion-parameter model on one RTX 5090 with at most 32 GB of RAM;
llama.cpp's open pull request 25294 streams experts from disk at 1.83 to
2.20 tokens/s for a 754B model with 55 to 79 GB of cache at 73 to 79% hit;
kTransformers (SOSP 2025, Apache-2.0) reaches 8.7 to 16 tokens/s only with
382 GB of DRAM and no paging.

Predictability claims diverge: ExpertFlow (arXiv 2410.17954) reports 92% hit
and up to 95% expert-prediction accuracy on Mixtral-class models; a
trace-driven study of a trillion-parameter model (arXiv 2608.12103) measures
64.7% router-lookahead recall, finds it worth 0.3% of median time as
kernel readahead advice, and finds kernel MGLRU within about 10% of an
oracle frequency policy. Pre-gated MoE (ISCA 2024) gives exact one-layer
lookahead but needs the model retrained. llama.cpp's GGUF interleaves
experts inside one tensor, so one expert touches pages of every expert; an
expert-contiguous re-layout cut page touches from 6,912 to 192 in one
reported case (discussion 27149).

Conclusion: no software makes a 600B-class model fast on a 32 to 64 GB PC
from NVMe; the honest target is 1 to 2 tokens/s at 2 to 4 bits, and
interactive speed for 30B to 120B MoEs. The bar for any userspace expert
cache is to beat MGLRU by more than 10% at equal memory under domain shift.

### H4 Encodings (12 sources)

* Lossless BF16 weight compression lands at about 30%: DFloat11 (NeurIPS
  2025, arXiv 2504.11651, Apache-2.0; GPU-only decoder) and ZipNN (arXiv
  2411.05239, MIT; 80 GB/s decode on 224 cores, about 1.7 GB/s single
  stream). FP8 ends at 55 to 70% of size; FP4 values are essentially
  incompressible (arXiv 2508.19263), which matches ddram's own measurement
  (Q4_K_M at 98.4% with zlib). One ISCA 2026 paper (arXiv 2606.15789)
  claims int4 and fp4 carry entropy below their bit width with tile-level
  ANS; no code was found and the int4 ratio is not in the abstract, so this
  is the one claim worth measuring.
* KV cache is where large factors are, and they are lossy and engine-side:
  KIVI (ICML 2024, MIT) 2-bit KV, 8× fewer bytes. SSD KV tiers (LMCache,
  MLSys 2026; Mooncake, FAST 2025; Dynamo KVBM; Tutti; py-kvcache; KVSwap,
  MobiSys 2026) are each an engine-specific connector with its own block
  store and per-block I/O issue path; py-kvcache shows SSD KV can lose to
  recompute on an H100 and that the break-even reuse rate falls from 77.8%
  at 8k tokens to 7.8% at 80k.

Conclusion: decode-on-load helps BF16/FP8 tiers only and costs 4 to 8 CPU
cores per NVMe drive; for GGUF it buys nothing. The missing substrate under
every KV tier is a declared read set with exact sequential prefetch.

### H5 Pushing the plan to the device (14 sources)

* Shipping: CXL DRAM expanders (Micron CZ120 128/256 GB; SK hynix CMM-DDR5)
  and Phison aiDAPTIV+ (PyTorch-bound middleware over SLC SSDs). NAND behind
  CXL (Samsung CMM-H) is an FPGA prototype in the 2025 literature (arXiv
  2503.22017) with no datasheet found. Kioxia's GP1 (10M IOPS at 512 B,
  samples by end of 2026, slipped once) optimizes the unplanned small-read
  path.
* Host-to-device intent today: exactly one live, standardized, Linux-plumbed
  channel, NVMe Flexible Data Placement (TP4146, ratified 2022; Linux 6.x,
  QEMU `fdp=on`), and it covers write placement only. The read side has the
  per-I/O Dataset Management hints, including access-frequency value 7
  ("speculative read, part of a prefetch"), which controllers "may" ignore,
  the Streams host code was removed from Linux, and no mainline path exposes
  read hints to userspace. CXL 3.2's Hot-range Monitoring Unit runs the other
  way (device tells host what is hot).
* Host-directed prefetch into device DRAM exists as prior art in academic
  FPGA work from 2026: ITME (arXiv 2606.12556) pairs a device-side prefetcher
  with a user-level prefetch API and reports 35.7% over CPU offloading;
  HyMCache (arXiv 2607.18141) offers an explicit issue/wait/release interface
  for KV objects. No product implements either.
* A bound on the read-side idea: an NVMe queue holding thousands of
  commands already conveys the host's near-term plan to the controller, and
  ddram's plan path is already at the device's sequential limit with 2 MiB
  coalesced reads (benchmarks.md). The per-I/O "speculative read" hint has
  existed since NVMe 1.0 and vendors ignored it, which is evidence that
  controllers saw no gain, not only that nobody asked. Consumer drives have
  0 to 4 GB of DRAM or none (host memory buffer), so pre-staging there adds
  a hop, not bandwidth. Any gain must come from NAND-side scheduling beyond
  what queue depth exposes, which is unmeasured.
* OS tiering ceiling, quantified: TPP +18% over stock Linux (ASPLOS 2023);
  state-of-the-art systems 2.3× off optimal once loaded latency inflates
  (Colloid, SOSP 2024); Nomad up to 6× over TPP under pressure but up to
  217% slowdown elsewhere; SOAR/ALTO up to 12.4× by abandoning pure hotness
  (OSDI 2025); learned prefetchers (Voyager, Memix, xTier) narrow the gap in
  simulation and never close it. Every one predicts the future from the
  past.

## 3. Contradictions worth knowing

| Topic | Source A | Source B | Status |
|---|---|---|---|
| Does expert prediction help? | PowerInfer-2, Edge0, ExpertFlow: 4 to 25×, 92 to 95% accuracy (coarse MoE, phones, unified memory) | WiSP and the trillion-scale kernel-tiering study: little gain, 64.7% recall worth 0.3% | Unresolved. No source varies link saturation with prediction held fixed; PowerInfer-2's baseline is llama.cpp's interleaved layout, so layout is a confound. A pager must accept a confidence, not a certainty, and measure. |
| Do 4-bit weights compress? | ISCA 2026 ANS paper: entropy below bit width | Intel FP4 study and ddram's zlib test: no | Unresolved; symbol-level vs byte-level codec. Measure on a GGUF. |
| Is GDS the fix for SSD KV? | NVIDIA, DeepNVMe | Tutti: GDS stalls on CPU issue for fragmented KV | One competitor's claim about another on different stacks; GDS removes the bounce buffer, the per-block issue cost is a separate question. |
| Does flash tiering pay on edge devices? | Phison, vendors (latency, capacity) | IEEE CAL 2025: 4 to 12× energy per token | Adverse for battery-powered targets; favourable only where energy is not the objective. |

A note on ddram's own numbers. The 3.7× on the chunked workload and the
halved MoE I/O (benchmarks.md) are measured against ddram v0.1, whose stream
readahead overshoots chunk and expert boundaries and whose automatic policy
flips between cyclic and CLOCK; the kernel reads 8.5 GB per pass on the same
workload where v0.1 reads 23.4 GB. A chunk-aware readahead cap would recover
most of that without a plan. The part attributable to Belady eviction alone
is the residual: 6.5 GB per pass against the kernel's 8.5 GB (about 1.3×)
plus exact prefetch. The MoE plan in that benchmark is also an oracle (the
router is deterministic and declared per token); real lookahead is 65 to 95%
accurate depending on the model. Both caveats go into the next benchmark
round: a fixed-readahead heuristic baseline, and plans with realistic
prediction error.

## 4. The gap, and what "revolutionary" would honestly mean

Every system found hard-codes three separable things inside one inference
engine or one vendor stack: a source of future knowledge (a router
prediction, a compiler lifetime analysis, an activation histogram), a
layout policy (co-locate what is consumed together), and a cost-aware
selection rule (utility per byte, bundle sizing, low-bit fallback). Nothing
exposes them as a general memory layer, and nothing carries that knowledge
past the host into the drive.

The gap the evidence identifies, which no source measures filling, is
**intent-driven storage for AI memory**: one declared or learned access plan
that flows from the application, through a model-agnostic host tier, down
to the SSD. Five proposals follow, each with what would prove it.

1. **Plan-driven re-layout (host, now).** Rewrite the backing file so that
   blocks consumed together are contiguous, from the recorded co-access
   trace. The mechanism is the most consistently reported one in the
   literature: Ripple's 1.49× end to end on sparse models, and a page-touch
   reduction from 6,912 to 192 per expert in one llama.cpp report (a
   page-touch count, not a tokens/s figure). It has only been shown with
   model knowledge; ddram's own learned plan lost 5 to 20% to the heuristic
   because the recorded trace is a four-thread interleaving, and a re-layout
   from that trace inherits the noise. Proof needed: an end-to-end tokens/s
   gain on llama.cpp from a trace-derived re-layout, with trace smoothing.
2. **Confidence-weighted plans (host API, cheap).** A plan entry carries a
   probability so the pager can prefetch what is likely and keep what is
   uncertain. Variants exist privately in ProMoE, ExpertFlow and HOBBIT.
   Proof needed: a measured gain over a binary hint; the trillion-scale
   study found 0.3% for 64.7%-recall lookahead and MGLRU within about 10%
   of an oracle frequency policy, so the ceiling is modest at that scale.
   Separate and model-aware, not part of a model-agnostic core: alternative
   representations per block (HOBBIT's low-bit fallback), which GGUF blocks
   cannot provide without a second file.
3. **Plan-coherent placement over FDP (host to device, today), for
   writable regions only.** Weights are mapped read-only and never written
   back, so FDP cannot help them. It applies to KV cache, activations and
   training state that ddram writes back. Blocks the plan evicts together
   get one placement handle. Standardized, in Linux and QEMU; the 2.5 to
   1.25 write-amplification figure is modeled on cache workloads, not
   measured for this one; FDP drives are enterprise parts today. Proof
   needed: write amplification on an FDP-capable drive with a KV-cache
   workload.
4. **A read-side plan hint (host to device): an open question, not a
   proposal yet.** The idea has prior art (ITME, HyMCache, and the
   never-honoured NVMe speculative-read hint), and its gain is bounded by
   what a deep queue with large coalesced reads already gives the
   controller. An honest emulation study must hold queue depth, I/O size and
   host-side coalescing equal in the baseline, model the controller's DRAM
   (or its absence) and die and channel count, and charge the host RAM the
   look-ahead pins. If a gain survives that, it is a case to bring to NVMe,
   Kioxia, Phison or Samsung; if not, the plan's value stays on the host.
5. **GPU-consumable tier (host, integration).** CPU-issued prefetch along
   the plan into the pinned arena with a per-block ready fence, and
   DirectStorage (Windows only) or cuFile-compatible back ends. The
   measured consumer-side gain for removing the bounce buffer is about 1.4×
   (DeepNVMe); G10's planned migration result is simulated. No GPU was
   available to validate anything here.

What this does not change: the arithmetic of 10 GB per token for a
671B-class model, the energy cost of flash versus HBM, and the hot-set
random-access gap to kernel swap. Any pitch that implies otherwise will not
survive a vendor's engineers.

## 5. Recommended next steps

In order of cost, each gated on its proof: (0) a chunk-aware readahead cap
in the heuristic path, as the baseline every plan result is measured against
from now on, plus plans with realistic prediction error in the MoE
benchmark; (1) re-layout from recorded plans with trace smoothing and a
before/after llama.cpp MoE benchmark; (2) confidence-weighted plans in the
API, measured against binary hints; (3) FDP placement from plan groups for
writable regions, validated in QEMU and measured on an FDP drive; (4) the
emulated-controller study of read-side hints with the controls above;
(5) the GPU ready-fence mode once a GPU is available.

## 6. Source list

Verified items are cited inline above with venue and arXiv identifier.
Licences recorded as reusable (MIT, Apache-2.0, BSD): PowerInfer, CATS, TEAL,
vlm-flash, Edge0, BaM, FlashNeuron, DeepSpeed, FlexGen, G10, Tutti, Dynamo,
DirectStorage, MoE-Infinity, Pre-gated MoE, Fiddler, kTransformers,
mixtral-offloading, DFloat11, ZipNN, Any-Precision LLM, InfiniGen, Mooncake,
LMCache, KIVI, py-kvcache, Colloid, SOAR/ALTO. Ideas only (no licence,
non-commercial, or no code): LLM in a flash, Deja Vu, PowerInfer-2, ProSparse
operators, Sirius, Ripple, ProMoE, AdapMoE, ExpertFlow, HOBBIT, SSD-LLaMA,
FlashMoE, NeuZip, MEMTIS, GIDS, GeminiFS, the ISCA 2026 ANS paper.
