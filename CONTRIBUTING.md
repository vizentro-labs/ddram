# Contributing to ddramnvme

Thanks for helping. ddramnvme is open source under the Apache License 2.0 and
is maintained by Vizentro Labs.

## Contributor License Agreement

Before we can merge your first pull request, you need to accept the
[Contributor License Agreement](CLA.md). A bot will ask you on the pull
request; reply with the sentence it gives you. You only sign once.

The CLA lets Vizentro Labs maintain, relicense and, if needed, transfer the
project while keeping every released version available under Apache-2.0. You
keep the copyright in your contributions, and the CLA includes a patent
license like the one in Apache-2.0.

Do not submit code copied from projects under incompatible licenses (GPL,
LGPL or non-commercial licenses). Reading papers and documentation for ideas
is fine; cite them in the docs.

## Workflow

1. Open an issue for anything larger than a bug fix, so the design can be
   discussed first (see `docs/architecture.md`).
2. `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
   and `cargo test --workspace --release` must pass. CI runs them on Linux
   (x86_64, arm64), macOS (Apple Silicon) and Windows.
3. Performance changes must include before/after numbers from `ddram bench`
   (or `bench/run.sh`) and the hardware they were measured on.
4. Changes to the fault path must keep it allocation-free and lock-free,
   because it runs inside signal and exception handlers.
5. New source files start with the license header:

   ```rust
   // Copyright 2026 Vizentro Labs
   // SPDX-License-Identifier: Apache-2.0
   ```

## Reporting results

Benchmarks on real NVMe drives and GPUs are especially welcome. Open an issue
with the `bench/run.sh` output, the `ddram smart` output (redact the serial
number), and the drive model, OS and kernel version.

## Code of conduct

Everyone participating in the project follows the
[Code of Conduct](CODE_OF_CONDUCT.md).
