// SPDX-License-Identifier: AGPL-3.0-only
//! OS adapters: capture and inject input. See `docs/ARCHITECTURE.md` §5.
//!
//! Translation tables are pure and compile on every OS, so CI tests them everywhere;
//! only the thin FFI layers are `cfg`-gated.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
mod folders;
pub mod input;
pub mod linux;
pub mod mac;
pub mod text;
mod version;
pub mod win;

pub use folders::downloads_dir;
pub use version::os_version;

/// A running clipboard-change watcher; dropping it stops the watcher thread.
pub struct ClipboardWatch(Option<Box<dyn FnOnce() + Send>>);

impl Drop for ClipboardWatch {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop();
        }
    }
}

/// Calls `on_change` from a background thread whenever the clipboard changes, for as long as
/// the result lives. `None` where the OS has no change notification (macOS, Wayland) or it
/// could not be set up: the caller polls instead.
pub fn watch_clipboard(on_change: Box<dyn Fn() + Send>) -> Option<ClipboardWatch> {
    #[cfg(windows)]
    let stop = win::ffi::watch_clipboard(on_change);
    #[cfg(free_unix)]
    let stop = linux::ffi::watch_clipboard(on_change);
    #[cfg(not(any(windows, free_unix)))]
    let stop = {
        let _ = on_change;
        None
    };
    stop.map(|s| ClipboardWatch(Some(s)))
}

/// A counter that changes whenever the clipboard's contents do (macOS `changeCount`, Windows
/// clipboard sequence number), so a reader can tell the clipboard moved under it. `None`
/// where the OS has no such counter.
pub fn clipboard_change_count() -> Option<i64> {
    #[cfg(target_os = "macos")]
    return Some(mac::ffi::clipboard_change_count());
    #[cfg(windows)]
    return Some(win::ffi::clipboard_change_count());
    #[cfg(not(any(target_os = "macos", windows)))]
    None
}

/// True when the current clipboard is marked private by the app that set it (password
/// managers do this for copied secrets), so it must not be shared. `false` on other OSes.
pub fn clipboard_is_private() -> bool {
    #[cfg(target_os = "macos")]
    return mac::ffi::clipboard_is_private();
    #[cfg(windows)]
    return win::ffi::clipboard_is_private();
    #[cfg(free_unix)]
    return linux::ffi::clipboard_is_private();
    #[cfg(not(any(target_os = "macos", windows, free_unix)))]
    false
}
