// SPDX-License-Identifier: AGPL-3.0-only
//! Becoming a session: trusting a pinned peer, exchanging `Hello` and agreeing on a Desk.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::controller::Side;
use overdesk_core::engine::Engine;
use overdesk_core::{DeskError, LayoutError, adopt};
use overdesk_net::pairing::{Fingerprint, peer_id};
use overdesk_net::session::{Me, PeerHello, SessionError, hello};
use overdesk_net::transport::{CloseReason, Link, LinkRx, LinkTx, Role, SETUP_TIMEOUT};
use overdesk_proto::{Control, Desk, DeskScreen, MAX_SCREEN_SIDE, Os, PeerId, log};

/// Why a connection could not become a running session.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The secure link, the trust check or the `Hello` exchange failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The Desk the dialer sent was refused by [`adopt`].
    #[error("peer did not send a valid Desk: {0}")]
    Desk(#[from] DeskError),
    /// The agreed Desk cannot be laid out on this device.
    #[error("the Desk cannot be laid out: {0}")]
    Layout(#[from] LayoutError),
    /// The dialer's Desk is not newer than the empty one, or a screen is not the size its owner announced.
    #[error("peer sent a Desk that does not match its Hello")]
    DeskMismatch,
    /// The peer sent a control message other than the one this step expects.
    #[error("unexpected message from the peer")]
    UnexpectedMessage,
    /// The peer is an Android TV, which only Overdesk Pro may share a Desk with.
    #[error("Android TV needs Overdesk Pro")]
    NeedsPro,
    /// The embedder's [`Admit`] hook turned the peer away, and the link was closed with this reason.
    #[error("peer refused: {0}")]
    Refused(CloseReason),
}

/// The future an [`Admit`] hook returns.
pub type AdmitFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), CloseReason>> + Send + 'a>>;

/// A generic hook an embedder may give the controller to vet a peer after `Hello` and before the
/// Desk is agreed. It gets the link, to exchange one message each way if it needs to, who the peer
/// said it is, and the peer's pinned certificate fingerprint. `Err(reason)` refuses the peer: the link
/// is closed with that [`CloseReason`], and the controller shows its own sentence for it as a notice. The open core
/// attaches no meaning to any of it; without a hook every pinned peer is admitted, as before.
pub trait Admit: Send + Sync {
    /// Vets `peer`, whose certificate has `fingerprint`, on `link`.
    fn admit<'a>(&'a self, link: &'a mut Link, peer: &'a Introduced, fingerprint: &'a Fingerprint) -> AdmitFuture<'a>;
}

/// A trusted, introduced peer with a shared Desk, ready to run.
pub struct Session {
    /// The cursor-routing engine for this device, built from the agreed Desk.
    pub engine: Engine,
    /// The sending half of the link: control messages and motion datagrams.
    pub tx: LinkTx,
    /// The receiving half of the link, read by the session's helper tasks.
    pub rx: LinkRx,
    /// This device's id, derived from its certificate fingerprint.
    pub me: PeerId,
    /// The peer's id, as it introduced itself in `Hello`.
    pub peer: PeerId,
    /// The peer's human-readable device name, shown in the UI.
    pub peer_name: String,
    /// The peer's operating system, which decides how keys are translated.
    pub peer_os: Os,
    /// The `FEATURE_*` bits the peer announced.
    pub peer_features: u64,
    /// The shared Desk both devices agreed on for this session.
    pub desk: Desk,
}

/// Who a peer said it is in `Hello`, after the trust check passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Introduced {
    /// The peer's id.
    pub id: PeerId,
    /// The peer's human-readable device name.
    pub name: String,
    /// The peer's operating system.
    pub os: Os,
    /// The peer's screen width in pixels.
    pub width: u32,
    /// The peer's screen height in pixels.
    pub height: u32,
    /// The `FEATURE_*` bits the peer announced; bits this build does not know are ignored.
    pub features: u64,
}

/// Trusts a pinned peer and exchanges `Hello`; nothing else is sent, so the caller
/// decides what Desk (if any) follows. The link stays whole.
pub async fn introduce(
    link: &mut Link,
    pinned: impl Fn(&Fingerprint) -> bool,
    me: &Me,
) -> Result<Introduced, DaemonError> {
    link.trust(pinned).map_err(SessionError::from)?;
    let PeerHello {
        peer,
        name,
        os,
        width,
        height,
        features,
    } = hello(link, me).await?;
    // Version, OS, screen and features only: nothing the peer typed or copied is ever in a Hello.
    log::info(format_args!(
        "peer introduced: {} \"{name}\", {}, screen {width}x{height}, protocol v{}, features {features:#x}",
        log::short_id(&peer),
        os.id(),
        overdesk_proto::PROTOCOL_VERSION
    ));
    Ok(Introduced {
        id: peer,
        name,
        os,
        width,
        height,
        features,
    })
}

/// How long [`establish`] may take in all once the link is up (the `Hello` exchange and the Desk).
const ESTABLISH_TIMEOUT: Duration = Duration::from_secs(SETUP_TIMEOUT.as_secs() * 2);

/// Trusts a pinned peer, exchanges `Hello`, refuses an Android TV peer (a Pro feature), agrees
/// on one Desk (the dialer authors it, the listener validates it with `adopt`), and splits the
/// link. A peer that stalls (say, never sends the Desk) ends it with [`SessionError::TimedOut`]
/// after twice [`SETUP_TIMEOUT`]. With an embedder's [`Admit`] hook, it runs after `Hello` and
/// before anything else is decided about the peer (the time limit covers it); a refusal is
/// [`DaemonError::Refused`].
pub async fn establish(
    link: Link,
    pinned: impl Fn(&Fingerprint) -> bool,
    me: &Me,
    side: Side,
    admit: Option<&dyn Admit>,
) -> Result<Session, DaemonError> {
    tokio::time::timeout(ESTABLISH_TIMEOUT, establish_inner(link, pinned, me, side, admit))
        .await
        .unwrap_or(Err(SessionError::TimedOut.into()))
}

/// [`establish`] without the time limit.
async fn establish_inner(
    mut link: Link,
    pinned: impl Fn(&Fingerprint) -> bool,
    me: &Me,
    side: Side,
    admit: Option<&dyn Admit>,
) -> Result<Session, DaemonError> {
    let peer = introduce(&mut link, pinned, me).await?;
    if let Some(admit) = admit {
        let fingerprint = link.peer_fingerprint();
        if let Err(why) = admit.admit(&mut link, &peer, &fingerprint).await {
            link.close(why);
            return Err(DaemonError::Refused(why));
        }
    }
    // The Free gate for Android TV. Pro's hub calls `introduce`, not `establish`, so it
    // is never subject to this check.
    if peer.os == Os::Android {
        return Err(DaemonError::NeedsPro);
    }
    let mine = peer_id(&link.local_fingerprint());
    let desk = match link.role() {
        Role::Dialer => {
            let desk = author_desk(mine, me, &peer, side)?;
            link.send_control(&Control::Desk(desk.clone()))
                .await
                .map_err(SessionError::from)?;
            desk
        }
        Role::Listener => {
            let Control::Desk(incoming) = link.recv_control().await.map_err(SessionError::from)? else {
                return Err(DaemonError::UnexpectedMessage);
            };
            adopt_incoming(incoming, mine, me, &peer)?
        }
    };
    let engine = Engine::try_new(mine, &desk, BTreeMap::from([(mine, me.os), (peer.id, peer.os)]))?;
    let (tx, rx) = link.split().map_err(SessionError::from)?;
    Ok(Session {
        engine,
        tx,
        rx,
        me: mine,
        peer: peer.id,
        peer_name: peer.name,
        peer_os: peer.os,
        peer_features: peer.features,
        desk,
    })
}

/// A screen of `peer` at `x`, named by its id: two Macs both called "MacBook" must not collide.
fn screen(peer: PeerId, x: i32, w: u32, h: u32) -> DeskScreen {
    DeskScreen {
        peer,
        name: peer.to_hex(),
        x,
        y: 0,
        w,
        h,
    }
}

/// The Desk the dialer proposes: its own screen at x = 0 and the peer's beside it on `side`.
fn author_desk(mine: PeerId, me: &Me, peer: &Introduced, side: Side) -> Result<Desk, DaemonError> {
    let out_of_range = |_| LayoutError::OutOfRange("screen".into());
    let peer_x = match side {
        Side::Right => i32::try_from(me.width).map_err(out_of_range)?,
        Side::Left => -i32::try_from(peer.width).map_err(out_of_range)?,
    };
    Ok(Desk {
        version: 1,
        author: mine,
        screens: vec![
            screen(mine, 0, me.width, me.height),
            screen(peer.id, peer_x, peer.width, peer.height),
        ],
    })
}

/// The Desk the dialer sent, if it is one this device may adopt: valid, authored by `peer`,
/// placing only the two of them, each screen the size its owner announced.
fn adopt_incoming(incoming: Desk, mine: PeerId, me: &Me, peer: &Introduced) -> Result<Desk, DaemonError> {
    // Nothing has been adopted yet, so any valid Desk from the dialer counts as newer.
    let empty = Desk {
        version: 0,
        author: PeerId([0; 16]),
        screens: vec![],
    };
    // Only this device and the introduced peer may own a screen in the incoming Desk.
    let adopted =
        adopt(&empty, incoming, peer.id, |p| *p == mine || *p == peer.id)?.ok_or(DaemonError::DeskMismatch)?;
    // Each screen must be the size its owner announced, or the Desk misplaces the edges.
    let sized = |s: &DeskScreen| {
        let (w, h) = if s.peer == mine {
            (me.width, me.height)
        } else {
            (peer.width, peer.height)
        };
        (s.w, s.h) == (w, h) && (1..=MAX_SCREEN_SIDE).contains(&s.w) && (1..=MAX_SCREEN_SIDE).contains(&s.h)
    };
    if adopted.screens.iter().all(sized) {
        Ok(adopted)
    } else {
        Err(DaemonError::DeskMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overdesk_core::DeskError;

    const MINE: PeerId = PeerId([1; 16]);
    const THEIRS: PeerId = PeerId([2; 16]);

    fn me() -> Me {
        Me {
            name: "me".into(),
            os: Os::MacOs,
            width: 1440,
            height: 900,
            features: 0,
        }
    }

    fn peer() -> Introduced {
        Introduced {
            id: THEIRS,
            name: "them".into(),
            os: Os::Windows,
            width: 1920,
            height: 1080,
            features: 0,
        }
    }

    #[test]
    fn the_dialer_places_the_peer_on_the_chosen_side() {
        let x_of = |side| author_desk(MINE, &me(), &peer(), side).unwrap().screens[1].x;
        assert_eq!(x_of(Side::Right), 1440);
        assert_eq!(x_of(Side::Left), -1920);
        let desk = author_desk(MINE, &me(), &peer(), Side::Right).unwrap();
        assert_eq!((desk.version, desk.author, desk.screens[0].x), (1, MINE, 0));
        assert_eq!(desk.screens[1].name, THEIRS.to_hex(), "screens are named by id");
    }

    #[test]
    fn a_screen_too_wide_for_the_desk_is_a_layout_error() {
        let mut huge = me();
        huge.width = u32::MAX;
        let err = author_desk(MINE, &huge, &peer(), Side::Right).unwrap_err();
        assert!(matches!(err, DaemonError::Layout(LayoutError::OutOfRange(_))), "{err}");
    }

    #[test]
    fn the_listener_adopts_the_desk_the_dialer_authored() {
        // The dialer is the peer: its screen at 0, this device's beside it, each as announced.
        let sent = Desk {
            version: 1,
            author: THEIRS,
            screens: vec![screen(THEIRS, 0, 1920, 1080), screen(MINE, 1920, 1440, 900)],
        };
        assert_eq!(adopt_incoming(sent.clone(), MINE, &me(), &peer()).unwrap(), sent);
    }

    #[test]
    fn the_listener_refuses_a_desk_that_misstates_a_size_or_is_not_the_senders() {
        let desk = |author, w| Desk {
            version: 1,
            author,
            screens: vec![screen(THEIRS, 0, w, 1080), screen(MINE, 1920, 1440, 900)],
        };
        let adopt = |d| adopt_incoming(d, MINE, &me(), &peer());
        assert!(adopt(desk(THEIRS, 1920)).is_ok());
        assert!(matches!(adopt(desk(THEIRS, 1000)), Err(DaemonError::DeskMismatch)));
        assert!(matches!(
            adopt(desk(MINE, 1920)),
            Err(DaemonError::Desk(DeskError::NotAuthor))
        ));
        let stranger = Desk {
            screens: vec![screen(PeerId([9; 16]), 0, 1920, 1080), screen(MINE, 1920, 1440, 900)],
            ..desk(THEIRS, 1920)
        };
        assert!(matches!(
            adopt(stranger),
            Err(DaemonError::Desk(DeskError::UnknownPeer(_)))
        ));
    }
}
