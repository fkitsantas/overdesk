// SPDX-License-Identifier: AGPL-3.0-only
//! What runs on a fresh link: pairing for unknown peers, `Hello` for trusted ones.
//! An untrusted link only ever decodes `Pairing`, never `Control`.

use overdesk_proto::{Control, Os};

use crate::pairing::{Fingerprint, PairError};
use crate::transport::{Link, NetError};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Net(#[from] NetError),
    #[error("pairing protocol: {0:?}")]
    Pairing(PairError),
    #[error("pairing declined")]
    Declined,
    #[error("peer speaks protocol {theirs}, we speak {ours}")]
    VersionMismatch { ours: u16, theirs: u16 },
    #[error("peer's Hello claims an id that does not match its certificate")]
    IdentityMismatch,
    #[error("expected Hello")]
    NoHello,
}

/// Which side of the connection we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Dialer,
    Listener,
}

/// Runs the 6-digit pairing. `confirm` shows the code and returns the user's answer.
/// Returns the peer fingerprint to pin, only if both users confirmed.
pub async fn pair(
    _link: &mut Link,
    _role: Role,
    _my_fp: Fingerprint,
    _confirm: impl AsyncFnOnce(u32) -> bool,
) -> Result<Fingerprint, SessionError> {
    todo!()
}

/// What we announce in `Hello`.
#[derive(Debug, Clone)]
pub struct Me {
    pub name: String,
    pub os: Os,
    pub width: u32,
    pub height: u32,
}

/// Exchanges `Hello` with a trusted peer. Returns the peer's `Hello`.
pub async fn hello(_link: &mut Link, _me: &Me) -> Result<Control, SessionError> {
    todo!()
}
