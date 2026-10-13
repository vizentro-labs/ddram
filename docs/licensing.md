# Licensing

## Summary

| | |
|---|---|
| License | Apache License 2.0 (`LICENSE`) |
| Copyright holder | Vizentro Labs (`NOTICE`, file headers) |
| Contributions | Accepted under the [Contributor License Agreement](../CLA.md) |
| Patents | None filed; the design is published openly. Apache-2.0 §3 and CLA §3 give every user a patent license from every contributor. |
| Dependencies | All `MIT OR Apache-2.0` (crossbeam-queue, crossbeam-utils, libc, windows-sys, windows-targets) |

The `Copyright [yyyy] [name of copyright owner]` line near the end of
`LICENSE` belongs to the license's appendix ("How to apply the Apache License
to your work") and stays as a template. The project's copyright notice is in
`NOTICE` and in the header of each source file:

```rust
// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0
```

## Why Apache-2.0

| Need | Apache-2.0 |
|---|---|
| Free use in commercial products (game engines, inference servers, OEM tools) | ✅ permissive |
| Explicit patent grant from every contributor, terminated for anyone who sues over the project | ✅ §3 |
| Clear inbound terms for contributions | ✅ §5, plus the CLA |
| No trademark rights granted with the code | ✅ §6 |
| Compatible with all current dependencies | ✅ |
| Compatible with GPLv3 projects | ✅ |
| Compatible with **GPLv2-only** projects (the Linux kernel, some drivers) | ❌ the patent-termination clause conflicts with GPLv2 |

If GPLv2 or Linux-kernel integration becomes a goal, the CLA allows Vizentro
Labs to add a license such as `MIT OR Apache-2.0` later without asking every
contributor again.

## Why a CLA

Contributors keep their copyright and grant Vizentro Labs a broad license to
their contributions, including the right to relicense and to transfer the
project to a successor. In return Vizentro Labs commits that every released
version stays available under its open-source license (CLA §5). Signatures
are collected by the CLA workflow (`.github/workflows/cla.yml`) and stored on
the `cla-signatures` branch.

## Third-party code and references

Ideas came from public specifications and papers. No code was copied. Do not
copy from these projects:

| Project | License | Status |
|---|---|---|
| Umap (LLNL userfaultfd pager) | LGPL-2.1 | ideas only |
| FastMap | GPL | ideas only |
| Memtis | CC BY-NC 4.0 | ideas only (non-commercial, incompatible) |
| FlexGen, DeepSpeed, vLLM, LMCache, Wasmtime | Apache-2.0 | compatible, with NOTICE attribution if code is ever reused |
| llama.cpp, DirectStorage samples | MIT | compatible, with attribution |
| smartmontools (SMART layouts reference) | GPL-2.0 | struct layouts are facts from the NVMe specification and Apple/Microsoft headers; ddram's readers were written from those specs, not from smartmontools code |

Specifications used: NVMe Base Specification 2.x (log page layouts), OCP
Datacenter NVMe SSD Specification, JEDEC JESD218/219 (endurance), Microsoft
and Apple public API documentation. `CODE_OF_CONDUCT.md` is the Contributor
Covenant 2.1, used under CC BY 4.0.

## Checklist for releases

* Run `cargo metadata` (or `cargo deny check licenses`) and confirm every
  dependency is still MIT/Apache/BSD/Zlib-compatible.
* Keep `NOTICE` up to date with any bundled third-party notices.
* Binary releases of the C library must ship `LICENSE` and `NOTICE`.
