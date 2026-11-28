// SPDX-License-Identifier: AGPL-3.0-only
//! Win32 capture (low-level hooks + Raw Input) and injection (`SendInput`). Same API as
//! the macOS adapter. Runs as a standard user: no service, no admin (ARCHITECTURE §8).
//! This file holds what the parts share (the error, the tag on our own input, the permissions);
//! `display`, `hooks`, `window`, `clipboard` and `inject` hold the rest.
#![allow(unsafe_code)] // FFI boundary; each unsafe block states its invariant.

mod clipboard;
mod display;
mod hooks;
mod inject;
mod window;

pub use clipboard::{clipboard_change_count, clipboard_is_private, watch_clipboard};
pub use display::{ensure_dpi_aware, main_display_size, monitors};
pub use hooks::{Capture, caps_lock_on};
pub use inject::Injector;

pub use crate::input::Captured;

/// Put in `dwExtraInfo` of everything we inject, so our own hooks skip it. Every peer
/// both captures and injects; without this, input would loop forever.
const INJECTED_TAG: usize = 0x4F56_4552; // "OVER"
/// `XBUTTON1` in `mouseData`: the first extra button, "Back" in browsers.
const XBUTTON1: u32 = 1;
/// `XBUTTON2` in `mouseData`: the second extra button, "Forward" in browsers.
const XBUTTON2: u32 = 2;

/// Why the Windows adapter could not start.
#[derive(Debug, PartialEq, Eq)]
pub enum WinError {
    /// `SetWindowsHookExW` failed for the low-level keyboard or mouse hook, or the
    /// capture thread could not be spawned or died before reporting in.
    HookFailed,
    /// The hidden message-only window that receives Raw Input could not be created.
    WindowFailed,
    /// `RegisterRawInputDevices` rejected the mouse registration.
    RawInputFailed,
}

impl std::fmt::Display for WinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::HookFailed => "the keyboard and mouse hooks could not be installed",
            Self::WindowFailed => "the window that receives raw input could not be created",
            Self::RawInputFailed => "the mouse could not be registered for raw input",
        })
    }
}

/// Windows needs no permission prompt for hooks or `SendInput` as a standard user.
pub fn permissions() -> (bool, bool) {
    (true, true)
}

/// Does nothing: Windows has no input permissions to request. Exists so both adapters
/// expose the same API.
pub fn request_permissions() {}

/// Encodes `s` as a NUL-terminated UTF-16 string for wide (`W`) Win32 APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
