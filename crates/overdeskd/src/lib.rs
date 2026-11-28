// SPDX-License-Identifier: AGPL-3.0-only
//! The Overdesk daemon: wires capture, the network and injection to the Desk engine.
//! The operating system is reached only through [`Platform`]; [`native`] holds the real
//! adapters and `main.rs` (or the app) picks one.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
mod clipboard;
pub mod controller;
mod env;
#[cfg(any(target_os = "macos", windows, free_unix))]
pub mod native;
mod platform;
mod redact;
mod run;
mod session;
mod task;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use clipboard::{ClipboardSync, clipboard_moved, read_checked};
pub use controller::Side;
pub use env::{escape_chord, load_identity, store_dir, this_os};
pub use overdesk_core::engine::Mode;
pub use overdesk_net::session::Me;
pub use overdesk_platform::text::KeyChar;
pub use platform::{Clipboard, Platform};
pub use redact::redact_action;
pub use run::{Ended, run, run_with, to_event};
pub use session::{Admit, AdmitFuture, DaemonError, Introduced, Session, establish, introduce};
