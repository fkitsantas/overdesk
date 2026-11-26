// SPDX-License-Identifier: AGPL-3.0-only
//! OS adapters: capture and inject input. See `docs/ARCHITECTURE.md` §5.
//!
//! Translation tables are pure and compile on every OS, so CI tests them everywhere;
//! only the thin FFI layers are `cfg`-gated.

pub mod input;
pub mod mac;
