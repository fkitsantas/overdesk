// SPDX-License-Identifier: AGPL-3.0-only
//! Windows adapter. `keymap` and `events` are pure translation; `ffi` talks to Win32.

pub mod events;
pub mod keymap;

#[cfg(windows)]
pub mod ffi;
