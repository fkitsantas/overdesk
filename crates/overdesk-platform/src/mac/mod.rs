// SPDX-License-Identifier: AGPL-3.0-only
//! macOS adapter. `keymap` and `events` are pure translation; `ffi` talks to Quartz.

pub mod events;
pub mod keymap;

#[cfg(target_os = "macos")]
pub mod ffi;
