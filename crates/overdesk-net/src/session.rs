// SPDX-License-Identifier: AGPL-3.0-only
//! What runs on a fresh link: pairing for unknown peers, `Hello` for trusted ones.
//! An untrusted link only ever decodes `Pairing`, never `Control`.

use overdesk_proto::{Control, Os, PROTOCOL_VERSION, Pairing};
use ring::rand::{SecureRandom, SystemRandom};

use crate::pairing::{Dialer, Fingerprint, Listener, Nonce, PairError, Step, peer_id};
use crate::transport::{Link, NetError, Role};

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

impl From<PairError> for SessionError {
    fn from(e: PairError) -> Self {
        Self::Pairing(e)
    }
}

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
pub async fn pair(link: &mut Link, confirm: impl AsyncFnOnce(u32) -> bool) -> Result<Fingerprint, SessionError> {
    let result = pair_inner(link, confirm).await;
    if matches!(result, Err(ref e) if !matches!(e, SessionError::Declined)) {
        link.close("pairing failed");
    }
    result
}

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
    pub name: String,
    pub os: Os,
    pub width: u32,
    pub height: u32,
}

/// Exchanges `Hello` with a trusted peer. Returns the peer's `Hello`.
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
