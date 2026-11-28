// SPDX-License-Identifier: AGPL-3.0-only
//! The Linux, FreeBSD and OpenBSD adapter's public API, dispatching at run time to the X11 or the Wayland backend.
//! Same surface as the macOS and Windows adapters.

use std::path::Path;

use crate::input::{Sink, Warnings};

use overdesk_proto::{Button, Gesture, Phase, ScrollUnit};

use super::wayland_core::{SessionKind, offers_private_hint, session_kind, wayland_supported_on};
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
    /// The session cannot change under a running process, so it is worked out once.
    static KIND: std::sync::OnceLock<SessionKind> = std::sync::OnceLock::new();
    *KIND.get_or_init(|| {
        let var = |k: &str| std::env::var(k).ok();
        session_kind(var("WAYLAND_DISPLAY").as_deref(), var("XDG_SESSION_TYPE").as_deref())
    })
}

/// The clear error for a Wayland session on an OS with no input portals (OpenBSD).
fn wayland_allowed() -> Result<(), LinuxError> {
    if wayland_supported_on(std::env::consts::OS) {
        return Ok(());
    }
    Err(LinuxError::Wayland(
        "Wayland sessions are not supported on OpenBSD (no input portals); log in to an X11 session".into(),
    ))
}

/// Whether this session is X11 (not Wayland).
pub(super) fn session_is_x11() -> bool {
    kind() == SessionKind::X11
}

/// Primary display size in pixels; `(0, 0)` when none is reachable. On Wayland it is known
/// once [`Capture::start_with`] has read the portal zones.
pub fn main_display_size() -> (u32, u32) {
    match kind() {
        SessionKind::Wayland => wayland::main_display_size(),
        SessionKind::X11 => x11::main_display_size(),
    }
}

/// Each monitor's rectangle relative to the desk box (see [`main_display_size`]); empty when unknown.
pub fn monitors() -> Vec<crate::input::Rect> {
    match kind() {
        SessionKind::Wayland => wayland::monitors(),
        SessionKind::X11 => x11::monitors(),
    }
}

/// Neither backend has a permission prompt the app can drive: X11 needs none, and the
/// Wayland portals ask for consent themselves when the session starts.
pub fn permissions() -> (bool, bool) {
    (true, true)
}

/// Does nothing; exists so every adapter exposes the same API.
pub fn request_permissions() {}

/// Reads the selection's offered MIME types over ext-data-control, else wlr-data-control
/// (KDE, Sway and wlroots, Hyprland). Fails closed (private) when neither protocol exists, as
/// on GNOME/Mutter, or the compositor can't be asked; an empty clipboard has nothing to leak.
fn wayland_private() -> bool {
    use wl_clipboard_rs::paste::{ClipboardType, Error, Seat, get_mime_types_ordered};
    match get_mime_types_ordered(ClipboardType::Regular, Seat::Unspecified) {
        Ok(mimes) => offers_private_hint(&mimes),
        Err(Error::ClipboardEmpty | Error::NoMimeType) => false,
        Err(_) => true,
    }
}

/// Whether the clipboard is marked private by the app that set it. On GNOME/Mutter (no
/// data-control protocol) Wayland sessions always report private, so nothing is sent outward.
pub fn clipboard_is_private() -> bool {
    match kind() {
        SessionKind::X11 => x11::clipboard_is_private(),
        SessionKind::Wayland => wayland_private(),
    }
}

/// Starts watching CLIPBOARD ownership changes (X11 only; Wayland has no such event, so the
/// caller polls). Returns the function that stops the watcher.
pub fn watch_clipboard(on_change: Box<dyn Fn() + Send>) -> Option<Box<dyn FnOnce() + Send>> {
    match kind() {
        SessionKind::X11 => x11::watch_clipboard(on_change),
        SessionKind::Wayland => None,
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
    /// Starts capture on whichever backend this session uses, delivering every event to `sink`
    /// on the capture thread.
    pub fn start_with(sink: Sink) -> Result<Self, LinuxError> {
        match kind() {
            SessionKind::X11 => x11::Capture::start_with(sink).map(Self::X11),
            SessionKind::Wayland => {
                wayland_allowed()?;
                wayland::Capture::start_with(sink).map(Self::Wayland)
            }
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
    /// Creates an injector for the primary display. On Wayland the portal's restore token
    /// lives in `token_dir` (the store folder), so the consent dialog shows once. Later
    /// trouble on Wayland is reported to `warnings`.
    pub fn new(token_dir: &Path, warnings: Warnings) -> Result<Self, LinuxError> {
        match kind() {
            SessionKind::X11 => x11::Injector::new().map(|i| Self::X11(Box::new(i))),
            SessionKind::Wayland => {
                wayland_allowed()?;
                wayland::Injector::new(token_dir, warnings).map(Self::Wayland)
            }
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
