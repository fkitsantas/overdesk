// SPDX-License-Identifier: AGPL-3.0-only
//! Linux capture and injection. Same API as the macOS and Windows adapters.
//!
//! shortcut: a skeleton that reports [`LinuxError::Unsupported`]; the X11 and Wayland
//! backends replace it.

use std::sync::mpsc::Receiver;

use overdesk_proto::{Button, Phase, ScrollUnit};

pub use crate::input::Captured;

/// Why the Linux adapter could not start.
#[derive(Debug, PartialEq, Eq)]
pub enum LinuxError {
    /// This session's display server is not supported yet.
    Unsupported,
}

/// Primary display size in pixels; `(0, 0)` until a backend can ask the display server.
pub fn main_display_size() -> (u32, u32) {
    (0, 0)
}

/// Linux needs no permission prompt on X11; Wayland asks through the portal when capture starts.
pub fn permissions() -> (bool, bool) {
    (true, true)
}

/// Does nothing: there are no input permissions to request ahead of time. Exists so every
/// adapter exposes the same API.
pub fn request_permissions() {}

/// A running capture. Dropping it stops capture.
pub struct Capture;

impl Capture {
    /// Starts capture and returns the receiver of captured input.
    pub fn start() -> Result<(Self, Receiver<Captured>), LinuxError> {
        Err(LinuxError::Unsupported)
    }

    /// While on, input is consumed instead of reaching local apps.
    pub fn set_swallow(&self, _on: bool) {}
}

/// Injects input into this session.
pub struct Injector;

impl Injector {
    /// Creates an injector for the primary display.
    pub fn new() -> Result<Self, LinuxError> {
        Err(LinuxError::Unsupported)
    }

    /// Where the system cursor is now (pixels).
    pub fn cursor() -> (f64, f64) {
        (0.0, 0.0)
    }

    /// Moves the pointer to primary-screen pixel coordinates.
    pub fn pointer(&mut self, _x: f64, _y: f64) {}

    /// Presses or releases `button` at the current cursor position.
    pub fn button(&mut self, _button: Button, _pressed: bool) {}

    /// Presses or releases the key for Overdesk `code`; `false` if this system has no such key.
    pub fn key(&mut self, _code: u32, _pressed: bool) -> bool {
        false
    }

    /// Scrolls by `dx`, `dy` in `unit`s.
    pub fn scroll(&mut self, _dx: i32, _dy: i32, _unit: ScrollUnit, _phase: Phase) {}

    /// Releases every key and button this injector holds down.
    pub fn release_all(&mut self) {}
}

/// True when the clipboard is marked private by the app that set it; `false` until a backend can tell.
pub fn clipboard_is_private() -> bool {
    false
}
