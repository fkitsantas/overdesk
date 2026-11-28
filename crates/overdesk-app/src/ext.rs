// SPDX-License-Identifier: AGPL-3.0-only
//! The one extension point of the app: a build may hand [`run_with`](crate::run_with) an
//! [`Extension`] that adds tray menu items and one status line to the window. The app renders
//! them and forwards the clicks; it knows nothing about what they do.

use overdeskd::controller::AppState;
use tokio::sync::mpsc;

/// An extra item of the tray menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItemSpec {
    /// Handed back to [`Extension::on_menu`] when the item is chosen; must differ from the app's own ids.
    pub id: String,
    /// The text of the item.
    pub label: String,
}

/// A quiet line at the bottom of the main page, with an optional button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    /// The line's text.
    pub text: String,
    /// The button beside it: its label and whether it can be pressed. Pressing calls [`Extension::on_button`].
    pub button: Option<(String, bool)>,
}

/// What an [`Extension`] asks of the app, from any thread.
#[derive(Debug, Clone)]
pub(crate) enum Request {
    /// Show or clear the status line.
    Status(Option<StatusLine>),
    /// Show this notice until the user dismisses it.
    Notice(String),
    /// Bring the window forward.
    ShowWindow,
    /// End the session cleanly and start the app again.
    Restart,
}

/// The extension's way to the app; cheap to clone, usable from any thread.
#[derive(Debug, Clone)]
pub struct Host(mpsc::UnboundedSender<Request>);

impl Host {
    /// A handle and the receiver the app drains.
    pub(crate) fn channel() -> (Self, mpsc::UnboundedReceiver<Request>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(tx), rx)
    }

    /// Shows `line` as the status line, or clears it with `None`.
    pub fn set_status(&self, line: Option<StatusLine>) {
        let _ = self.0.send(Request::Status(line));
    }

    /// Shows `text` as a notice until the user dismisses it.
    pub fn notice(&self, text: impl Into<String>) {
        let _ = self.0.send(Request::Notice(text.into()));
    }

    /// Brings the window forward.
    pub fn show_window(&self) {
        let _ = self.0.send(Request::ShowWindow);
    }

    /// Ends any session and starts the app again, so a replaced binary runs.
    pub fn restart(&self) {
        let _ = self.0.send(Request::Restart);
    }
}

/// What a build adds to the app. Every method has an empty default; none may block, since the
/// app calls them on its UI thread. Slow work belongs on a thread of the extension's own.
pub trait Extension: Send + Sync {
    /// Runs first thing in [`run`](crate::run), before the window. `host` reaches the app later.
    fn startup(&self, _host: Host) {}
    /// The extra items of the tray menu.
    fn menu(&self) -> Vec<MenuItemSpec> {
        Vec::new()
    }
    /// The item with this id was chosen.
    fn on_menu(&self, _id: &str) {}
    /// The status line's button was pressed.
    fn on_button(&self) {}
    /// The controller published `state`, or the app has just received its first one.
    fn on_state(&self, _state: &AppState) {}
}
