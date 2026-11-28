// SPDX-License-Identifier: AGPL-3.0-only
//! The one way a process tells the user something went wrong without stopping: a line on
//! stderr with the `overdesk:` prefix. Library crates never print; they return the condition and
//! the binary (daemon, app) hands it here.

/// Writes `what` to stderr as `overdesk: <what>`.
pub fn warn(what: impl std::fmt::Display) {
    eprintln!("overdesk: {what}");
}
