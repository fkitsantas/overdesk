// SPDX-License-Identifier: AGPL-3.0-only
//! What the daemon needs from the operating system: the [`Platform`] trait, whose clipboard half is
//! the [`Clipboard`] trait.

use std::path::PathBuf;

use overdesk_core::Rect;
use overdesk_core::engine::Action;
use overdesk_platform::input::Captured;
use overdesk_platform::text::KeyChar;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// The local clipboard. Every method has a default that touches nothing (tests, headless), so an
/// implementation overrides only what its OS offers. [`Platform`] requires it, and code that only
/// reads or writes the clipboard (the clipboard pump, `read_checked`) asks for just this.
pub trait Clipboard: Send + 'static {
    /// The clipboard's current text, or `None` if it is empty, holds something else or
    /// cannot be read. May block; `run` calls it off the async threads. The default
    /// (tests, headless) never touches a real clipboard.
    fn clipboard_text(&self) -> Option<String> {
        None
    }
    /// Whether the clipboard's current copy is marked private (a password manager's secret),
    /// so it must not be shared. May block; the default (tests, headless) is `false`.
    fn clipboard_is_private(&self) -> bool {
        false
    }
    /// Replaces the clipboard's contents with `text`. May block; same default as above.
    fn set_clipboard_text(&self, _text: &str) {}
    /// The clipboard's current image as `(width, height, RGBA bytes)`, or `None` if it
    /// holds none. Open-core plumbing for Pro: `run` never calls it, so Free stays text-only.
    fn clipboard_image(&self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }
    /// Replaces the clipboard's contents with an RGBA image. Pro plumbing, like above.
    fn set_clipboard_image(&self, _width: u32, _height: u32, _rgba: Vec<u8>) {}
    /// The files currently copied in the file manager, or `None`. Pro plumbing, like above.
    fn clipboard_files(&self) -> Option<Vec<PathBuf>> {
        None
    }
    /// Replaces the clipboard's contents with a list of files. Pro plumbing, like above.
    fn set_clipboard_files(&self, _files: &[PathBuf]) {}
    /// Calls `on_change` from another thread whenever the local clipboard changes, until the
    /// returned guard is dropped. `None` (the default) means the OS has no notification and
    /// `run` polls.
    fn watch_clipboard(&self, _on_change: Box<dyn Fn() + Send>) -> Option<Box<dyn Send>> {
        None
    }
    /// A counter that changes with the clipboard's contents, so polling can skip reads while
    /// it is unchanged and a read can be checked for a concurrent copy. `None` (the default)
    /// means every poll reads the contents and a read is confirmed by reading twice.
    fn clipboard_change_count(&self) -> Option<i64> {
        None
    }
}

/// What the OS adapter must do for the engine; its clipboard is the [`Clipboard`] half.
pub trait Platform: Clipboard {
    /// Turns swallowing on or off: while on, local input drives the peer and must not
    /// also reach this device.
    fn swallow(&self, on: bool);
    /// Performs `Inject*` and `ReleaseAll` actions.
    fn inject(&self, action: Action);
    /// This device's monitors, relative to its screen on the Desk, when they do not fill it
    /// (an L-shaped layout). The default, none, treats the screen as one monitor.
    fn monitors(&self) -> Vec<Rect> {
        Vec::new()
    }
    /// This device's main display size in pixels as it is now, read at the start of every
    /// session so a display change (a monitor plugged in, a new resolution) reaches the peer
    /// without restarting. `None` (the default: tests, headless) keeps the size the device
    /// started with.
    fn display_size(&self) -> Option<(u32, u32)> {
        None
    }
    /// Tells the capture side which plain local moves the engine can do without. `Some(monitors)`
    /// (the engine's own, box-relative) is published while no peer is driven or driving: a move
    /// that ends deep inside a monitor cannot hand the cursor to a peer, so it need not be sent.
    /// `None` withdraws it: every move is sent. Called at the start of a session, on every cursor
    /// mode change and when the session ends. The default does nothing (everything is sent).
    fn set_move_filter(&self, _monitors: Option<&[Rect]>) {}
    /// Local input for the next session. Platforms without capture (tests, headless)
    /// return a stream that ends immediately.
    fn capture(&self) -> UnboundedReceiver<Captured> {
        unbounded_channel().1
    }
    /// What the key `code` (just captured) types under this device's own keyboard layout, so a
    /// receiver with no keyboard (an Android TV) types it right on any layout. Pro only: the
    /// open daemon never calls it. Call it for every captured key, in order, so modifiers are
    /// tracked; `translate` is false while no receiver wants text, which also drops a
    /// half-typed dead key. The default (tests, headless, Wayland) is `None`: send the key.
    fn key_text(&self, _code: u32, _pressed: bool, _translate: bool) -> Option<KeyChar> {
        None
    }
    /// Reads the OS's real modifier and Caps Lock state into the character lookup. Call it
    /// when translation starts (the cursor moves onto a receiver that wants text): keys
    /// swallowed since the last time never reached the OS, so the lookup's own Caps Lock
    /// state drifts from it, and a modifier held before the cursor crossed over is unknown
    /// to it. Pro only. The default does nothing.
    fn sync_key_state(&self) {}
}
