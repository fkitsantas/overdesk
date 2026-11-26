// SPDX-License-Identifier: AGPL-3.0-only
//! What runs on a fresh link: pairing for unknown peers, `Hello` for trusted ones.
//! An untrusted link only ever decodes `Pairing`, never `Control`.

use overdesk_proto::{Control, Os, PROTOCOL_VERSION, Pairing};
use ring::rand::{SecureRandom, SystemRandom};

use crate::pairing::{Dialer, Fingerprint, Listener, Nonce, PairError, Step, peer_id};
use crate::transport::{Link, NetError, Role};

/// Errors from the session handshakes run on a fresh link: pairing and `Hello`.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The transport failed: the link closed, a frame was rejected, or the link is not
    /// trusted.
    #[error(transparent)]
    Net(#[from] NetError),
    /// The peer broke the pairing protocol: a message out of order, or a reveal that
    /// does not match its commitment. Treat it as a possible attack.
    #[error("pairing protocol: {0:?}")]
    Pairing(PairError),
    /// The exchange completed but at least one user rejected the code. Nothing is
    /// pinned.
    #[error("pairing declined")]
    Declined,
    /// The peer's `Hello` names a protocol version other than ours.
    #[error("peer speaks protocol {theirs}, we speak {ours}")]
    VersionMismatch {
        /// The version this build speaks.
        ours: u16,
        /// The version the peer announced.
        theirs: u16,
    },
    /// The peer id in the peer's `Hello` is not derived from the certificate it
    /// authenticated with, so it is trying to pass as another device.
    #[error("peer's Hello claims an id that does not match its certificate")]
    IdentityMismatch,
    /// The first control message from the peer was not `Hello`.
    #[error("expected Hello")]
    NoHello,
    /// Pairing did not finish within its time limit.
    #[error("pairing timed out")]
    TimedOut,
}

/// Covers the whole exchange including the user's answer, so a silent or stalled peer
/// cannot hold a connection (and a prompt) open; keep-alives defeat the idle timeout.
pub const PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl From<PairError> for SessionError {
    fn from(e: PairError) -> Self {
        Self::Pairing(e)
    }
}

/// 32 bytes from the operating system's CSPRNG, for one pairing attempt.
///
/// # Errors
///
/// Returns [`NetError::Io`] if the system RNG cannot be read; pairing never falls back
/// to a weaker source.
fn fresh_nonce() -> Result<Nonce, SessionError> {
    let mut n = [0u8; 32];
    SystemRandom::new()
        .fill(&mut n)
        .map_err(|_| NetError::Io("system RNG unavailable".into()))?;
    Ok(n)
}

/// Runs the 6-digit pairing. `confirm` shows the code and returns the user's answer.
/// Returns the peer fingerprint to pin, only if both users confirmed. Protocol
/// failures close the link. A clean decline leaves it open, because closing at once could
/// discard our `Reject` before the peer reads it; the caller drops the link.
///
/// The link is not trusted by this call: the caller pins the returned fingerprint and
/// then calls [`Link::trust`].
///
/// # Errors
///
/// Returns [`SessionError::Declined`] if either user rejects the code,
/// [`SessionError::Pairing`] if the peer breaks the protocol,
/// [`SessionError::TimedOut`] after [`PAIRING_TIMEOUT`], and [`SessionError::Net`] if
/// the link fails.
pub async fn pair(link: &mut Link, confirm: impl AsyncFnOnce(u32) -> bool) -> Result<Fingerprint, SessionError> {
    pair_with_timeout(link, confirm, PAIRING_TIMEOUT).await
}

/// [`pair`] with an explicit time limit.
///
/// # Errors
///
/// As for [`pair`], with [`SessionError::TimedOut`] after `limit`.
pub async fn pair_with_timeout(
    link: &mut Link,
    confirm: impl AsyncFnOnce(u32) -> bool,
    limit: std::time::Duration,
) -> Result<Fingerprint, SessionError> {
    let result = tokio::time::timeout(limit, pair_inner(link, confirm))
        .await
        .unwrap_or(Err(SessionError::TimedOut));
    // Close on every failure except a decline, whose `Reject` must still reach the peer.
    if matches!(result, Err(ref e) if !matches!(e, SessionError::Declined)) {
        link.close("pairing failed");
    }
    result
}

/// The pairing exchange itself, without the time limit or close-on-failure handling.
/// Both sides always send their own verdict and wait for the peer's, so neither pins
/// unless both users confirmed.
///
/// # Errors
///
/// As for [`pair`], except [`SessionError::TimedOut`].
async fn pair_inner(link: &mut Link, confirm: impl AsyncFnOnce(u32) -> bool) -> Result<Fingerprint, SessionError> {
    let (my_fp, peer_fp) = (link.local_fingerprint(), link.peer_fingerprint());
    let nonce = fresh_nonce()?;
    let code = match link.role() {
        Role::Dialer => {
            let (mut d, commit) = Dialer::new(nonce, my_fp, peer_fp);
            link.send_pairing(&commit).await?;
            match d.on_message(&link.recv_pairing().await?)? {
                Step::SendAndShow(reveal, code) => {
                    link.send_pairing(&reveal).await?;
                    code
                }
                _ => return Err(PairError::Unexpected.into()),
            }
        }
        Role::Listener => {
            // The fingerprints are passed in role order, so both sides hash the same input.
            let mut l = Listener::new(nonce, peer_fp, my_fp);
            let Step::Send(reply) = l.on_message(&link.recv_pairing().await?)? else {
                return Err(PairError::Unexpected.into());
            };
            link.send_pairing(&reply).await?;
            let Step::Show(code) = l.on_message(&link.recv_pairing().await?)? else {
                return Err(PairError::Unexpected.into());
            };
            code
        }
    };
    let ok = confirm(code).await;
    link.send_pairing(&if ok { Pairing::Confirm } else { Pairing::Reject })
        .await?;
    let theirs = link.recv_pairing().await?;
    if ok && theirs == Pairing::Confirm {
        Ok(peer_fp)
    } else {
        Err(SessionError::Declined)
    }
}

/// What we announce in `Hello`.
#[derive(Debug, Clone)]
pub struct Me {
    /// Human-readable device name shown to the peer's user.
    pub name: String,
    /// Operating system of this device, so the peer can map keys and shortcuts.
    pub os: Os,
    /// Width of this device's screen in pixels.
    pub width: u32,
    /// Height of this device's screen in pixels.
    pub height: u32,
}

/// Exchanges `Hello` with a trusted peer. Returns the peer's `Hello`.
///
/// The peer id we announce is derived from our certificate, and the peer's is checked
/// against the certificate it authenticated with, so a pinned device cannot claim to be
/// another one.
///
/// # Errors
///
/// Returns [`SessionError::Net`] if the link is untrusted or fails,
/// [`SessionError::NoHello`] if the peer's first message is not `Hello`,
/// [`SessionError::VersionMismatch`] for another protocol version, and
/// [`SessionError::IdentityMismatch`] for a forged peer id.
pub async fn hello(link: &mut Link, me: &Me) -> Result<Control, SessionError> {
    link.send_control(&Control::Hello {
        version: PROTOCOL_VERSION,
        peer: peer_id(&link.local_fingerprint()),
        name: me.name.clone(),
        os: me.os,
        width: me.width,
        height: me.height,
    })
    .await?;
    let theirs = link.recv_control().await?;
    let Control::Hello { version, peer, .. } = &theirs else {
        return Err(SessionError::NoHello);
    };
    if *version != PROTOCOL_VERSION {
        return Err(SessionError::VersionMismatch {
            ours: PROTOCOL_VERSION,
            theirs: *version,
        });
    }
    if *peer != peer_id(&link.peer_fingerprint()) {
        return Err(SessionError::IdentityMismatch);
    }
    Ok(theirs)
}
