// SPDX-License-Identifier: AGPL-3.0-only
//! One QUIC connection per peer pair: a reliable control stream plus motion datagrams,
//! over mutual TLS 1.3 with self-signed device certificates.
//!
//! - `error`: [`NetError`] and the wire codes of [`CloseReason`].
//! - `identity`: [`Identity`] and the verifier that leaves trust to the pinned fingerprint.
//! - `node`: [`Node`], dialing and accepting under the handshake caps.
//! - `link`: [`Link`], the authenticated connection and its frame IO.
//! - `halves`: [`LinkTx`], [`LinkRx`], [`Closer`] and the motion and transfer handles.

mod error;
mod halves;
mod identity;
mod link;
mod node;

pub(crate) use error::io;
pub use error::{CloseReason, NetError};
pub use halves::{Closer, LinkRx, LinkTx, MotionRx, MotionSender, TransferOpener, TransferRx, TransferWriter};
pub use identity::Identity;
pub use link::{Link, Role};
pub use node::{ALPN, Node, SETUP_TIMEOUT};
