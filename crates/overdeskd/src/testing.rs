// SPDX-License-Identifier: AGPL-3.0-only
//! Fake platforms for tests (the `testing` feature): the daemon, the controller and Pro's crates
//! all need a [`Platform`] that touches no real input, and should not each write their own.

use std::sync::{Arc, Mutex};

use overdesk_core::engine::Action;

use crate::{Clipboard, Platform};

/// A platform that does nothing, for tests that only watch what the controller publishes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Silent;

impl Clipboard for Silent {}

impl Platform for Silent {
    fn swallow(&self, _on: bool) {}
    fn inject(&self, _action: Action) {}
}

/// A platform that remembers what it was asked to do and holds a text clipboard, for tests that
/// assert what a session did. Clones share everything.
#[derive(Debug, Clone, Default)]
pub struct Recording {
    /// Every action injected, in order.
    pub log: Arc<Mutex<Vec<Action>>>,
    /// Whether local input is being swallowed now.
    pub swallowing: Arc<Mutex<bool>>,
    /// The clipboard's text: written by the session, and by a test to "copy" something.
    pub clipboard: Arc<Mutex<Option<String>>>,
    /// The monitors last published through [`Platform::set_move_filter`]; `None` while withdrawn.
    pub move_filter: Arc<Mutex<Option<Vec<overdesk_core::Rect>>>>,
}

impl Clipboard for Recording {
    fn clipboard_text(&self) -> Option<String> {
        self.clipboard.lock().unwrap().clone()
    }
    fn set_clipboard_text(&self, text: &str) {
        *self.clipboard.lock().unwrap() = Some(text.into());
    }
}

impl Platform for Recording {
    fn swallow(&self, on: bool) {
        *self.swallowing.lock().unwrap() = on;
    }
    fn inject(&self, action: Action) {
        self.log.lock().unwrap().push(action);
    }
    fn set_move_filter(&self, monitors: Option<&[overdesk_core::Rect]>) {
        *self.move_filter.lock().unwrap() = monitors.map(<[_]>::to_vec);
    }
}
