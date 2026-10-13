// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! Thin OS abstraction: frame arena, address-space reservation, block
//! mapping and the process-wide fault handler.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
