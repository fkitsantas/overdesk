// SPDX-License-Identifier: AGPL-3.0-only
//! The Linux adapter's public API, dispatching at run time to the X11 or the Wayland backend.
//! Same surface as the macOS and Windows adapters.

use std::sync::mpsc::Receiver;

use overdesk_proto::{Button, Gesture, Phase, ScrollUnit};

use super::wayland_core::{SessionKind, session_kind};
use super::{wayland, x11};
pub use crate::input::Captured;

/// Why the Linux adapter could not start.
#[derive(Debug, PartialEq, Eq)]
pub enum LinuxError {
    /// No `DISPLAY` is set, so there is no X server to talk to.
    NoDisplay,
    /// Connecting to the X server failed, or it lacks a required extension; carries the reason.
    X11(String),
    /// The Wayland portals are unavailable or refused; the text says why and that X11 is not
    /// used as a fallback.
    Wayland(String),
}

impl std::fmt::Display for LinuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDisplay => f.write_str("no DISPLAY is set"),
            Self::X11(e) | Self::Wayland(e) => f.write_str(e),
        }
    }
}

/// Which backend this session needs.
fn kind() -> SessionKind {
    let var = |k: &str| std::env::var(k).ok();
    session_kind(var("WAYLAND_DISPLAY").as_deref(), var("XDG_SESSION_TYPE").as_deref())
}

/// Primary display size in pixels; `(0, 0)` when none is reachable. On Wayland it is known
/// once [`Capture::start`] has read the portal zones.
pub fn main_display_size() -> (u32, u32) {
    match kind() {
        SessionKind::Wayland => wayland::main_display_size(),
        SessionKind::X11 => x11::main_display_size(),
    }
}

/// Neither backend has a permission prompt the app can drive: X11 needs none, and the
/// Wayland portals ask for consent themselves when the session starts.
pub fn permissions() -> (bool, bool) {
    (true, true)
}

/// Does nothing; exists so every adapter exposes the same API.
pub fn request_permissions() {}

/// Whether the clipboard is marked private by the app that set it.
pub fn clipboard_is_private() -> bool {
    match kind() {
        SessionKind::X11 => x11::clipboard_is_private(),
        // Fail closed: no private-copy hint is read on Wayland yet, so a password manager's copy
        // can't be told apart and nothing is sent from here (receiving still works).
        // shortcut: no outbound clipboard on Wayland, until the offered MIME types are checked
        // for x-kde-passwordManagerHint through ext-data-control.
        SessionKind::Wayland => true,
    }
}

/// A running capture on whichever backend this session uses. Dropping it stops capturing.
pub enum Capture {
    /// Xorg.
    X11(x11::Capture),
    /// Wayland portals.
    Wayland(wayland::Capture),
}

impl Capture {
    /// Starts capture and returns the receiver of captured input.
    pub fn start() -> Result<(Self, Receiver<Captured>), LinuxError> {
        match kind() {
            SessionKind::X11 => x11::Capture::start().map(|(c, rx)| (Self::X11(c), rx)),
            SessionKind::Wayland => wayland::Capture::start().map(|(c, rx)| (Self::Wayland(c), rx)),
        }
    }

    /// While on, local apps stop receiving input; see the backend for how.
    pub fn set_swallow(&self, on: bool) {
        match self {
            Self::X11(c) => c.set_swallow(on),
            Self::Wayland(c) => c.set_swallow(on),
        }
    }
}

/// Injects input on whichever backend this session uses.
pub enum Injector {
    /// Xorg (XTest).
    X11(Box<x11::Injector>),
    /// Wayland (RemoteDesktop portal).
    Wayland(wayland::Injector),
}

impl Injector {
    /// Creates an injector for the primary display.
    pub fn new() -> Result<Self, LinuxError> {
        match kind() {
            SessionKind::X11 => x11::Injector::new().map(|i| Self::X11(Box::new(i))),
            SessionKind::Wayland => wayland::Injector::new().map(Self::Wayland),
        }
    }

    /// Where the system cursor is now (pixels); `(0, 0)` if unknown.
    pub fn cursor() -> (f64, f64) {
        match kind() {
            SessionKind::X11 => x11::Injector::cursor(),
            SessionKind::Wayland => wayland::Injector::cursor(),
        }
    }

    /// Moves the pointer to primary-screen pixel coordinates.
    pub fn pointer(&mut self, x: f64, y: f64) {
        match self {
            Self::X11(i) => i.pointer(x, y),
            Self::Wayland(i) => i.pointer(x, y),
        }
    }

    /// Presses or releases `button`.
    pub fn button(&mut self, button: Button, pressed: bool) {
        match self {
            Self::X11(i) => i.button(button, pressed),
            Self::Wayland(i) => i.button(button, pressed),
        }
    }

    /// Presses or releases a key; `false` for codes Linux has no key for.
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        match self {
            Self::X11(i) => i.key(code, pressed),
            Self::Wayland(i) => i.key(code, pressed),
        }
    }

    /// Scrolls by `dx`, `dy` in `unit`s.
    pub fn scroll(&mut self, dx: i32, dy: i32, unit: ScrollUnit, phase: Phase) {
        match self {
            Self::X11(i) => i.scroll(dx, dy, unit, phase),
            Self::Wayland(i) => i.scroll(dx, dy, unit, phase),
        }
    }

    /// Replays a trackpad gesture where Linux has a public equivalent (pinch as Ctrl+wheel).
    pub fn gesture(&mut self, gesture: Gesture, phase: Phase) {
        match self {
            Self::X11(i) => i.gesture(gesture, phase),
            Self::Wayland(i) => i.gesture(gesture, phase),
        }
    }

    /// Releases every key and button this injector holds down.
    pub fn release_all(&mut self) {
        match self {
            Self::X11(i) => i.release_all(),
            Self::Wayland(i) => i.release_all(),
        }
    }
}
