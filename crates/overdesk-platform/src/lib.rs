// SPDX-License-Identifier: AGPL-3.0-only
//! OS adapters: capture and inject input. See `docs/ARCHITECTURE.md` §5.
//!
//! Translation tables are pure and compile on every OS, so CI tests them everywhere;
//! only the thin FFI layers are `cfg`-gated.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
pub mod input;
pub mod linux;
pub mod mac;
pub mod win;

/// True when the current clipboard is marked private by the app that set it (password
/// managers do this for copied secrets), so it must not be shared. `false` on other OSes.
pub fn clipboard_is_private() -> bool {
    #[cfg(target_os = "macos")]
    return mac::ffi::clipboard_is_private();
    #[cfg(windows)]
    return win::ffi::clipboard_is_private();
    #[cfg(target_os = "linux")]
    return linux::ffi::clipboard_is_private();
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    false
}
