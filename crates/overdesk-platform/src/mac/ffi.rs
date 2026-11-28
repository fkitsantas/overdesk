// SPDX-License-Identifier: AGPL-3.0-only
//! Quartz event tap (capture) and event posting (inject). Kept thin: every decision
//! lives in the pure `events` / `keymap` modules. This file holds what the parts share (the
//! error, the permissions, the tag on our own events); `display`, `tap`, `inject` and
//! `clipboard` hold the rest.
#![allow(unsafe_code)] // FFI boundary; each unsafe block states its invariant.

mod clipboard;
mod display;
mod inject;
mod pointing;
mod tap;

pub use clipboard::{clipboard_change_count, clipboard_is_private};
pub use display::{main_display_size, monitors};
pub use inject::Injector;
pub use pointing::pointing_features;
pub use tap::Capture;

use objc2_core_graphics::{
    CGPreflightListenEventAccess, CGPreflightPostEventAccess, CGRequestListenEventAccess, CGRequestPostEventAccess,
};

/// Stamped into `kCGEventSourceUserData` of every event we post, so our own tap ignores
/// it. Every peer both captures and injects; without this, input would loop forever.
const INJECTED_TAG: i64 = 0x4F56_4552_4445_534B; // "OVERDESK"

/// Why the macOS adapter could not start.
#[derive(Debug, PartialEq, Eq)]
pub enum MacError {
    /// Grant in System Settings › Privacy & Security › Input Monitoring.
    NoListenPermission,
    /// Grant in System Settings › Privacy & Security › Accessibility.
    NoPostPermission,
    /// `CGEventTapCreate`, its run-loop source or the tap thread could not be set up,
    /// usually because a permission was revoked after the preflight check.
    TapCreateFailed,
    /// `CGEventSourceCreate` returned no event source for injection.
    SourceCreateFailed,
}

impl std::fmt::Display for MacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoListenPermission => "Input Monitoring is not allowed",
            Self::NoPostPermission => "Accessibility is not allowed",
            Self::TapCreateFailed => "the keyboard and mouse event tap could not be created",
            Self::SourceCreateFailed => "the event source for typing could not be created",
        })
    }
}

/// `(may capture, may inject)` without prompting.
pub fn permissions() -> (bool, bool) {
    (CGPreflightListenEventAccess(), CGPreflightPostEventAccess())
}

/// Shows the system permission prompts for anything not yet granted.
pub fn request_permissions() {
    let (listen, post) = permissions();
    if !listen {
        CGRequestListenEventAccess();
    }
    if !post {
        CGRequestPostEventAccess();
    }
}

pub use crate::input::Captured;
