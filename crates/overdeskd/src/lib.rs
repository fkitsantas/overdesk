// SPDX-License-Identifier: AGPL-3.0-only
//! The Overdesk daemon: wires capture, the network and injection to the Desk engine.
//! Platform-free; `main.rs` supplies the OS adapter.

use overdesk_core::engine::{Action, Engine, Event};
use overdesk_net::pairing::Fingerprint;
use overdesk_net::session::{Me, SessionError};
use overdesk_net::transport::{Link, LinkRx, LinkTx};
use overdesk_platform::input::Captured;
use overdesk_proto::PeerId;
use tokio::sync::mpsc::UnboundedReceiver;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("peer did not send a valid Desk: {0}")]
    BadDesk(String),
}

/// Where the peer's screen sits relative to ours (the dialer decides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// A trusted, introduced peer with a shared Desk, ready to run.
pub struct Session {
    pub engine: Engine,
    pub tx: LinkTx,
    pub rx: LinkRx,
    pub peer: PeerId,
    pub peer_name: String,
}

/// What the OS adapter must do for the engine.
pub trait Platform: Send + 'static {
    fn swallow(&self, on: bool);
    /// Performs `Inject*` and `ReleaseAll` actions.
    fn inject(&self, action: Action);
}

/// Trusts a pinned peer, exchanges `Hello`, agrees on one Desk (the dialer authors it,
/// the listener validates it with `adopt`), and splits the link.
pub async fn establish(
    _link: Link,
    _pinned: impl Fn(&Fingerprint) -> bool,
    _me: &Me,
    _side: Side,
) -> Result<Session, DaemonError> {
    todo!()
}

/// Runs a session until the peer is lost. Leaves the platform safe: not swallowing,
/// nothing held.
pub async fn run(_session: Session, _local: UnboundedReceiver<Captured>, _platform: impl Platform) {
    todo!()
}

/// Platform capture event to engine event.
pub fn to_event(c: Captured) -> Event {
    match c {
        Captured::Moved { dx, dy, x, y } => Event::LocalMove { dx, dy, x, y },
        Captured::Button { button, pressed } => Event::LocalButton { button, pressed },
        Captured::Key { code, pressed } => Event::LocalKey { code, pressed },
        Captured::Scroll { dx, dy, unit, phase } => Event::LocalScroll { dx, dy, unit, phase },
        Captured::Escape => Event::Escape,
    }
}
