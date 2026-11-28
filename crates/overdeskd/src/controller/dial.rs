// SPDX-License-Identifier: AGPL-3.0-only
//! One connection attempt, from a link to the end of its session.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use overdesk_core::engine::Mode;
use overdesk_net::pairing::Fingerprint;
use overdesk_net::session::{Me, SessionError};
use overdesk_net::sync::Relock;
use overdesk_net::transport::{CloseReason, Link, NetError};
use overdesk_proto::PeerId;
use overdesk_proto::log::{self, short_id};
use tokio::time::Instant;

use super::model::{DeviceKind, PeerView, Phase, ScreenView, Side};
use super::persist::save_side;
use super::shared::{Shared, focus};
use super::trust::{Direction, Unpaired, check_expected, pair_with_user};
use crate::{DaemonError, Ended, Platform, Session, establish, run_with};

/// How long to leave a paired device alone after an ordinary failed automatic dial.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// How long after an automatic dial reached the wrong device (stale or spoofed mDNS).
const RETRY_AFTER_UNEXPECTED: Duration = Duration::from_secs(60);

/// What the user reads when a VPN is up and a computer cannot be reached.
pub const VPN_HINT: &str =
    "Your VPN may be blocking your local network. Turn on Allow LAN access (or Local network sharing) in your VPN app.";

/// An automatic dial's context: who it targets and the phase to return to if it fails,
/// so a failed attempt never leaves an error screen.
pub(super) struct Auto {
    /// The paired device being dialed.
    pub(super) peer: PeerId,
    /// The phase the user was on before the attempt (`Idle` or `Lost`).
    pub(super) restore: Phase,
}

/// What a person should read for a failed dial, in place of the transport's wording. Every
/// sentence is this device's own: nothing the peer wrote is ever shown.
fn friendly(e: &NetError) -> &'static str {
    match e {
        NetError::Refused(CloseReason::NotPaired) => {
            "Open Overdesk on the other computer and click Pair a device, then try again."
        }
        NetError::Refused(CloseReason::Busy) => "The other computer is busy with another connection.",
        NetError::Refused(CloseReason::NeedsPro) => "That device needs a computer running Overdesk Pro.",
        NetError::Untrusted => "This computer isn't paired with that one. Pair them first.",
        _ => {
            "Couldn't reach the other computer. Check that Overdesk is open there and that both are on the same network."
        }
    }
}

/// What the notice reads on a device whose [`crate::Admit`] hook turned a peer away. Written here, never by
/// the peer.
fn turned_away(why: CloseReason) -> &'static str {
    match why {
        CloseReason::NeedsPro => "A computer without Overdesk Pro tried to connect to this device.",
        _ => "A device was turned away.",
    }
}

/// Logs a failed dial once, where it is handled, and returns what the user reads.
fn dial_failed(e: &NetError) -> String {
    log::warn(format_args!("connection failed: {e}"));
    friendly(e).to_owned()
}

/// The screens of `desk` as `WxH at x,y` each: the layout, which says nothing of what is on them.
fn desk_text(desk: &overdesk_proto::Desk) -> String {
    let screens: Vec<String> = desk
        .screens
        .iter()
        .map(|s| format!("{}x{} at {},{}", s.w, s.h, s.x, s.y))
        .collect();
    screens.join("; ")
}

/// One connection attempt, from a link to the end of its session. All state writes are
/// dropped once `generation` is stale.
pub(super) struct Dial<P> {
    /// State shared with the controller.
    shared: Arc<Shared<P>>,
    /// The generation this attempt may publish under.
    generation: u64,
    /// `Some` for an automatic reconnect, whose failures are silent.
    auto: Option<Auto>,
    /// [`PinBook::generation`](super::trust::PinBook::generation) when the attempt began; a Forget since then makes it stale.
    trust: u64,
}

impl<P: Platform + Clone + Sync> Dial<P> {
    /// An attempt starting now under `generation`.
    pub(super) fn new(shared: Arc<Shared<P>>, generation: u64, auto: Option<Auto>) -> Self {
        let trust = shared.book.generation();
        Self {
            shared,
            generation,
            auto,
            trust,
        }
    }

    /// Publishes `phase`, with a new Desk view if given.
    fn set(&self, phase: Phase, desk: Option<Vec<ScreenView>>) {
        self.shared.set(self.generation, phase, desk);
    }

    /// Publishes an error screen.
    fn error(&self, msg: String) {
        self.set(Phase::Error(msg), None);
    }

    /// Reports a failed dial: an error screen, or for an automatic one a quiet retreat that
    /// leaves the peer alone for `wait`.
    fn fail(&self, msg: String, wait: Duration) {
        // An automatic retry every few seconds would fill the log; its raw error is logged once already.
        if self.auto.is_none() {
            log::info(format_args!("attempt failed: {msg}"));
        }
        match &self.auto {
            None => self.error(msg),
            Some(a) => {
                self.shared.retry_after.relock().insert(a.peer, Instant::now() + wait);
                self.set(a.restore.clone(), None);
            }
        }
    }

    /// [`Dial::fail`] for a computer that could not be reached, adding [`VPN_HINT`] while a VPN is up:
    /// to a manual dial's error, and once (until a session connects) as a notice for an automatic one.
    fn unreachable(&self, msg: String, wait: Duration) {
        let shared = &self.shared;
        let vpn = (shared.vpn)();
        if vpn && self.auto.is_some() && !shared.vpn_warned.swap(true, Ordering::SeqCst) {
            shared.notify(VPN_HINT.to_owned());
        }
        self.fail(
            if vpn && self.auto.is_none() {
                format!("{msg} {VPN_HINT}")
            } else {
                msg
            },
            wait,
        );
    }

    /// Pairs if needed, establishes the Desk, runs until the peer is lost.
    /// `Inbound` connections from unknown devices pair only while pairing is open. A dial with
    /// `expect` set connects only to a pinned peer with that id and never falls back to
    /// pairing. An automatic dial that fails restores the previous phase and backs off
    /// instead of showing an error.
    pub(super) async fn run(
        self,
        link: Result<Link, NetError>,
        side: Side,
        direction: Direction,
        expect: Option<PeerId>,
    ) {
        let mut link = match link {
            Ok(l) => l,
            // Only dials this device made are worth an error screen.
            Err(_) if direction == Direction::Inbound => return,
            Err(e) => return self.unreachable(dial_failed(&e), RETRY_AFTER),
        };
        let peer_fp = link.peer_fingerprint();
        if let Err(unexpected) = check_expected(expect, &peer_fp, &self.shared.pins()) {
            // Someone else answered at the announced address: never offer to pair with them.
            link.close(CloseReason::UnexpectedPeer);
            return self.fail(unexpected.to_string(), RETRY_AFTER_UNEXPECTED);
        }
        // An unknown device: pairing is the only way in.
        if !self.shared.pins().contains(&peer_fp) && !self.pair(&mut link, direction).await {
            return;
        }
        let Some(session) = self.establish(link, side, &peer_fp).await else {
            return;
        };
        self.serve(session, &peer_fp).await;
    }

    /// Pairs with the device on `link` after the user confirms the code. `false` when the
    /// attempt is over (refused, declined or failed), with the screen already set.
    async fn pair(&self, link: &mut Link, direction: Direction) -> bool {
        let shared = &self.shared;
        let on_paired = || {
            // Close the window only if this pairing used it, not the user's own dial.
            if direction == Direction::Inbound {
                shared.set_pairing(None);
            }
        };
        match pair_with_user(
            link,
            direction,
            &shared.book,
            &shared.window,
            |p| self.set(p, None),
            on_paired,
        )
        .await
        {
            Ok(()) => {
                shared.refresh_nearby();
                true
            }
            Err(Unpaired::Refused) => {
                self.set(Phase::Idle, None);
                false
            }
            Err(Unpaired::Failed(msg)) => {
                self.error(msg);
                false
            }
        }
    }

    /// Exchanges `Hello` and agrees on a Desk. `None` when it failed or the peer was forgotten
    /// meanwhile, with the screen already set.
    async fn establish(&self, link: Link, side: Side, peer_fp: &Fingerprint) -> Option<Session> {
        let shared = &self.shared;
        // Read the pins again: pairing may have just added this peer.
        let pins = shared.pins();
        // The display may have changed since the last session (a monitor, a resolution): announce it as it is now.
        let me = session_me(&shared.me, shared.platform.display_size());
        let s = match establish(link, |fp| pins.contains(fp), &me, side, shared.admit.as_deref()).await {
            Ok(s) => s,
            Err(DaemonError::Session(SessionError::TimedOut)) => {
                return self.unreachable_none("The other computer didn't answer in time.".into());
            }
            Err(DaemonError::Session(SessionError::Net(e))) => return self.unreachable_none(dial_failed(&e)),
            // The embedder's hook turned the peer away: say why, whoever dialed.
            Err(DaemonError::Refused(why)) => {
                shared.notify(turned_away(why).to_owned());
                self.set(Phase::Idle, None);
                return None;
            }
            // Not transient: leave the TV alone for a while instead of redialing every few seconds.
            Err(e @ DaemonError::NeedsPro) => {
                if self.auto.is_some() {
                    // An automatic dial is silent, but this one will never work: say why.
                    shared.notify(format!("{e}. It stays disconnected."));
                }
                return self.fail_none(e.to_string(), RETRY_AFTER_UNEXPECTED);
            }
            Err(e) => return self.fail_none(format!("Couldn't set up the Desk: {e}"), RETRY_AFTER),
        };
        // Forgotten while setting up: drop the link instead of connecting with the old trust.
        if !shared.book.still_trusted(self.trust, peer_fp) {
            self.set(Phase::Idle, None);
            return None;
        }
        Some(s)
    }

    /// [`Dial::fail`], for a caller that returns `None`.
    fn fail_none<T>(&self, msg: String, wait: Duration) -> Option<T> {
        self.fail(msg, wait);
        None
    }

    /// [`Dial::unreachable`] after [`RETRY_AFTER`], for a caller that returns `None`.
    fn unreachable_none<T>(&self, msg: String) -> Option<T> {
        self.unreachable(msg, RETRY_AFTER);
        None
    }

    /// Publishes the connected session, runs it until the peer is lost, then publishes how it ended.
    async fn serve(&self, s: Session, peer_fp: &Fingerprint) {
        let (shared, generation) = (&self.shared, self.generation);
        // The name the peer gave over the authenticated link is the one to keep.
        shared.learn_name(s.peer, &s.peer_name);
        shared.learn_os(s.peer, s.peer_os);
        // Connected again: a later failure under a VPN is news again.
        shared.vpn_warned.store(false, Ordering::SeqCst);
        *shared.closer.relock() = Some(s.tx.closer());
        // Remember which side the peer's screen is on, for automatic reconnects.
        let x = |p: PeerId| s.desk.screens.iter().find(|d| d.peer == p).map(|d| d.x);
        if let (Some(mine), Some(theirs)) = (x(s.me), x(s.peer)) {
            let side = if theirs > mine { Side::Right } else { Side::Left };
            shared.saved(save_side(&shared.dir, &s.peer, side));
        }
        let peer_id = s.peer;
        let peer = s.peer_name.clone();
        log::info(format_args!(
            "session with {} started; desk: {}",
            short_id(&peer_id),
            desk_text(&s.desk)
        ));
        self.set(
            Phase::Connected {
                peers: vec![PeerView {
                    id: s.peer,
                    name: peer.clone(),
                    os: s.peer_os,
                    features: s.peer_features,
                    kind: DeviceKind::Computer,
                    label: None,
                }],
                mode: Mode::Local,
            },
            Some(desk_view(&s, &shared.me.name)),
        );
        let watcher = shared.clone();
        let on_mode = move |mode: Mode| {
            watcher.state.send_if_modified(|st| {
                if watcher.generation.load(Ordering::SeqCst) != generation {
                    return false;
                }
                // Mode changes only patch a running session; a later phase must never be overwritten.
                if let Phase::Connected { mode: m, .. } = &mut st.phase {
                    *m = mode;
                }
                let at = focus(&st.phase, &st.desk);
                let changed = st.focus != at || matches!(st.phase, Phase::Connected { .. });
                st.focus = at;
                changed
            });
        };
        let noticer = shared.clone();
        let on_notice = move |text: String| {
            if noticer.generation.load(Ordering::SeqCst) == generation {
                noticer.notify(text);
            }
        };
        let ended = run_with(
            s,
            shared.platform.capture(),
            shared.platform.clone(),
            on_mode,
            on_notice,
        )
        .await;
        // The link is gone: a later Disconnect must not "leave" a dead connection, or a newer one.
        if shared.generation.load(Ordering::SeqCst) == generation {
            *shared.closer.relock() = None;
        }
        log::info(format_args!("session with {} ended: {ended:?}", short_id(&peer_id)));
        match ended {
            Ended::PeerLeft => {
                shared.left.relock().insert(peer_id);
            }
            // Only a pinned peer may unpair us (the link was authenticated against the pins when it was set up).
            Ended::Forgotten if shared.pins().contains(peer_fp) => {
                shared.unpair(peer_id);
                self.set(Phase::Idle, Some(Vec::new()));
                return shared.notify(format!("{peer} forgot this computer. Pair again to reconnect."));
            }
            Ended::Forgotten | Ended::Lost => {}
        }
        self.set(Phase::Lost { peer }, Some(Vec::new()));
    }
}

/// `base` with the display size read for this session, if the platform reports one.
pub fn session_me(base: &Me, size: Option<(u32, u32)>) -> Me {
    let mut me = base.clone();
    if let Some((width, height)) = size {
        me.width = width;
        me.height = height;
    }
    // Read per session: a Magic Mouse may have been paired since the last one.
    #[cfg(any(target_os = "macos", windows, free_unix))]
    {
        me.features |= crate::native::input_features();
    }
    me
}

/// Turns the session's Desk into screens named for people, looking each owner up among
/// this device and its peers (an unknown owner falls back to its hex id).
fn desk_view(s: &Session, my_name: &str) -> Vec<ScreenView> {
    let names = BTreeMap::from([(s.me, my_name.to_string()), (s.peer, s.peer_name.clone())]);
    s.desk
        .screens
        .iter()
        .map(|d| ScreenView {
            id: d.peer,
            name: names.get(&d.peer).cloned().unwrap_or_else(|| d.peer.to_hex()),
            x: d.x,
            y: d.y,
            w: d.w,
            h: d.h,
            me: d.peer == s.me,
            online: true,
            kind: DeviceKind::Computer,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use overdesk_proto::Os;

    #[test]
    fn this_sessions_display_size_replaces_the_starting_one() {
        let base = Me {
            name: "m".into(),
            os: Os::MacOs,
            width: 1,
            height: 2,
            features: 1 << 40,
        };
        let me = session_me(&base, Some((1440, 900)));
        assert_eq!((me.width, me.height, me.features & 1 << 40), (1440, 900, 1 << 40));
        assert_eq!((session_me(&base, None).width, base.width), (1, 1));
    }

    #[test]
    fn transport_errors_become_plain_advice() {
        let refused = |r| friendly(&NetError::Refused(r));
        assert!(refused(CloseReason::NotPaired).contains("click Pair a device"));
        assert!(refused(CloseReason::Busy).contains("busy"));
        assert!(refused(CloseReason::NeedsPro).contains("Overdesk Pro"));
        let lost = friendly(&NetError::Closed("timed out".into()));
        assert!(lost.contains("Couldn't reach") && !lost.contains("timed out"));
        assert_eq!(
            friendly(&NetError::Refused(CloseReason::Other)),
            lost,
            "an unknown reason is a generic failure"
        );
    }
}
