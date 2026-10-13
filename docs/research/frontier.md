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
| H5 | Push the plan to the device | Open white space. No product lets a host declare future reads to an SSD; one write-side channel (FDP) exists today. |

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
  not energy.

What transfers to a model-agnostic pager: the offline re-layout pass
(Ripple) needs only an observed co-access trace, which ddram already records;
the utility-per-byte selection rule (VLM in a flash) needs only per-block
read cost, which ddram can measure. Neither needs model knowledge.

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
* The only host-directed prefetch into device DRAM found anywhere is
  academic FPGA work from 2026 (ITME, arXiv 2606.12556; HyMCache, arXiv
  2607.18141).
* OS tiering ceiling, quantified: TPP +18% over stock Linux (ASPLOS 2023);
  state-of-the-art systems 2.3× off optimal once loaded latency inflates
  (Colloid, SOSP 2024); Nomad up to 6× over TPP under pressure but up to
  217% slowdown elsewhere; SOAR/ALTO up to 12.4× by abandoning pure hotness
  (OSDI 2025); learned prefetchers (Voyager, Memix, xTier) narrow the gap in
  simulation and never close it. Every one predicts the future from the
  past.

## 3. Contradictions worth knowing

| Topic | Source A | Source B | Resolution |
|---|---|---|---|
| Does expert prediction help? | PowerInfer-2, Edge0, ExpertFlow: 4 to 25×, 92 to 95% accuracy (coarse MoE, phones) | WiSP (arXiv 2606.21868), kernel-tiering study: little gain, 64.7% recall at trillion scale | Conditional: helps when the link, not the prediction, is idle, and on coarse-grained MoEs; the pager must accept a confidence, not a certainty. |
| Do 4-bit weights compress? | ISCA 2026 ANS paper: entropy below bit width | Intel FP4 study and ddram's zlib test: no | Unresolved; symbol-level vs byte-level codec. Measure on a GGUF. |
| Is GDS the fix for SSD KV? | NVIDIA, DeepNVMe | Tutti: GDS stalls on CPU issue for fragmented KV | Both: GDS removes the bounce buffer, not the per-block CPU issue cost. |
| Does flash tiering save money? | Phison, vendors | IEEE CAL 2025: 4 to 12× energy per token | Latency and energy are different objectives; state which one a result optimizes. |

## 4. The gap, and what "revolutionary" would honestly mean

Every system found hard-codes three separable things inside one inference
engine or one vendor stack: a source of future knowledge (a router
prediction, a compiler lifetime analysis, an activation histogram), a
layout policy (co-locate what is consumed together), and a cost-aware
selection rule (utility per byte, bundle sizing, low-bit fallback). Nothing
exposes them as a general memory layer, and nothing carries that knowledge
past the host into the drive.

The step change the evidence supports is therefore not a faster pager. It is
**intent-driven storage for AI memory**: one declared or learned access plan
that flows from the application, through a model-agnostic host tier, down
to the SSD controller.

1. **Plan-driven re-layout (host, now).** Rewrite the backing file so that
   blocks consumed together are contiguous, from the recorded co-access
   trace. Generic, model-agnostic, and the single largest concrete win in the
   evidence (page touches per expert 6,912 to 192 in llama.cpp; Ripple 1.3
   to 1.7× on phones). Measurable with the existing benchmarks.
2. **Probabilistic plans and alternative representations (host API).** A
   plan entry carries a confidence and an optional cheaper representation
   of the same block (low-bit fallback, HOBBIT-style), so the pager can
   prefetch what is likely and degrade instead of stall. This is the
   interface the whole MoE literature reimplements privately.
3. **Plan-coherent placement over FDP (host to device, today).** Blocks
   that the plan evicts and rewrites together get one placement handle, so
   the drive erases them as a unit. Standardized, in Linux and QEMU; the
   write-amplification win is modeled at about 2× lifetime and feeds the
   existing SMART governor. Only ddram has a plan to derive the grouping
   from.
4. **A read-side plan hint (host to device, proposal).** The dual of CXL's
   hot-range monitor: the host tells the drive the next N ranges in plan
   order, the controller pre-stages them in its DRAM or SLC and schedules
   NAND dies accordingly. No product does this; the spec's prefetch hint bit
   exists and is ignored. This is a small, TP-sized ask to NVMe, Kioxia,
   Phison or Samsung, and the piece that a chip vendor, not a software
   project, can turn into a product. It can be prototyped and quantified in
   an emulated controller before any silicon.
5. **GPU-consumable tier (host, integration).** Prefetch along the plan into
   the pinned arena with per-block ready fences and DirectStorage or
   cuFile-compatible back ends, so a consumer GPU gets G10-style planned
   migration without datacenter drivers.

What this does not change: the arithmetic of 10 GB per token for a
671B-class model, the energy cost of flash versus HBM, and the hot-set
random-access gap to kernel swap. Any pitch that implies otherwise will not
survive a vendor's engineers.

## 5. Recommended next release

v0.3, in order of evidence strength and cost: (1) re-layout from recorded
plans with a before/after llama.cpp MoE benchmark; (2) confidence-weighted
plans and block alternatives in the API; (3) FDP placement from plan groups,
validated in QEMU and measured for write amplification on an FDP-capable
drive; (4) an emulated-controller study of read-side plan hints as the
basis for a standards proposal; (5) the GPU ready-fence mode once a GPU is
available for validation.

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
