// SPDX-License-Identifier: AGPL-3.0-only
//! Helpers for tasks the daemon spawns.

use tokio::task::JoinHandle;

/// Aborts its task when dropped, so a session that ends (or whose future is dropped) leaves
/// no helper keeping the connection or a stale reader alive.
pub(crate) struct AbortOnDrop(pub(crate) JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
