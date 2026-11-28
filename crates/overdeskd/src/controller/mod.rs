// SPDX-License-Identifier: AGPL-3.0-only
//! What the app runs: one long-lived task that listens, dials, pairs and runs sessions.
//! The UI sends [`Command`]s and renders the [`AppState`] it publishes; nothing else.

mod dial;
mod model;
mod nearby;
pub mod persist;
mod shared;
pub mod trust;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overdesk_core::engine::Action;
use overdesk_net::discovery::{Advertiser, Browser, Change};
use overdesk_net::pairing::peer_id;
use overdesk_net::store::Store;
use overdesk_net::sync::Relock;
use overdesk_net::transport::{CloseReason, Link, Node};
use overdesk_proto::PeerId;
use overdesk_proto::log;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::Platform;
use crate::task::AbortOnDrop;
use dial::{Auto, Dial};
use persist::{load_names, load_oses, load_side};
use shared::Shared;
use trust::{Direction, PairingWindow, PinBook};

pub use dial::{VPN_HINT, session_me};
pub use model::{
    AppState, Command, Config, DeviceKind, FileOffer, NearbyDevice, PAIRING_WINDOW, PairedDevice, PeerView, Phase,
    Placement, ScreenView, Side, StartError, UnknownSide, ends_attempt,
};
pub use nearby::{apply, auto_target, nearby_list, paired_list};

/// How often the controller looks for a paired device to reconnect to.
const AUTO_INTERVAL: Duration = Duration::from_secs(2);

/// Starts the controller. Returns its command sender and live state.
pub async fn spawn<P: Platform + Clone + Sync>(
    config: Config,
    platform: P,
) -> Result<(mpsc::UnboundedSender<Command>, watch::Receiver<AppState>), StartError> {
    // The identity may sit in the OS keychain, which can block: keep it off the async threads.
    let dir = config.store_dir.clone();
    let (store, identity) = tokio::task::spawn_blocking(move || {
        let store = Store::open(&dir).map_err(|e| StartError::Store(e.to_string()))?;
        let identity = crate::load_identity(&store).map_err(|e| StartError::Store(e.to_string()))?;
        Ok::<_, StartError>((store, identity))
    })
    .await
    .map_err(|e| StartError::Store(e.to_string()))??;
    let node = Node::bind(&identity, config.listen).map_err(|e| StartError::Bind(e.to_string()))?;
    log::info(format_args!(
        "controller listening on {}; discovery {}",
        node.local_addr().map_or_else(|e| e.to_string(), |a| a.to_string()),
        if config.discovery { "on" } else { "off" }
    ));
    let (state, state_rx) = watch::channel(AppState {
        address: node.local_addr().ok(),
        ..AppState::new(config.me.name.clone(), config.me.os)
    });
    let shared = Arc::new(Shared {
        state,
        dir: config.store_dir.clone(),
        closer: Mutex::new(None),
        left: Mutex::new(BTreeSet::new()),
        retry_after: Mutex::new(BTreeMap::new()),
        network: Notify::new(),
        book: PinBook::new(config.store_dir.clone(), Arc::new(store)),
        names: Mutex::new(load_names(&config.store_dir)),
        oses: Mutex::new(load_oses(&config.store_dir)),
        window: PairingWindow::default(),
        generation: AtomicU64::new(0),
        save_warned: AtomicBool::new(false),
        vpn_warned: AtomicBool::new(false),
        vpn: overdesk_net::discovery::vpn_active,
        found: Mutex::new(BTreeMap::new()),
        me: config.me,
        admit: config.admit,
        platform,
    });
    // Devices paired in an earlier run are listed before anything is heard.
    shared.refresh_nearby();
    // A device that cannot use mDNS (no interface) has no nearby list; it is told so.
    let started = config.discovery.then(|| -> Result<_, String> {
        let port = node.local_addr().map_err(|e| e.to_string())?.port();
        let id = peer_id(&identity.fingerprint());
        let advertiser = Advertiser::start(id, &shared.me.name, shared.me.os, port).map_err(|e| e.to_string())?;
        let (browser, changes) = Browser::start(id).map_err(|e| e.to_string())?;
        let shared = shared.clone();
        // Ends when the browser drops with the controller and closes the channel.
        std::thread::spawn(move || {
            while let Ok(change) = changes.recv() {
                if change == Change::NetworkChanged {
                    log::info(format_args!("network changed; VPN active: {}", (shared.vpn)()));
                    shared.network.notify_one();
                    continue;
                }
                if let Change::Lost(id) = &change {
                    // Gone from the network: a later reappearance is a fresh start.
                    shared.left.relock().remove(id);
                }
                shared.hear(change);
            }
        });
        Ok((advertiser, browser))
    });
    let discovery = match started {
        Some(Ok(d)) => {
            log::info(format_args!("discovery started; VPN active: {}", (shared.vpn)()));
            Some(d)
        }
        Some(Err(e)) => {
            log::warn(format_args!("discovery unavailable: {e}"));
            shared.notify("Couldn't look for other computers on your network. You can still type an address.".into());
            None
        }
        None => None,
    };
    let (commands, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        control(Arc::new(node), shared, rx, peer_id(&identity.fingerprint())).await;
        drop(discovery); // announced and browsing for exactly as long as the controller runs
    });
    Ok((commands, state_rx))
}

/// The controller's main loop state: at most one session task runs at a time.
struct Supervisor<P> {
    /// The listening socket and dialer.
    node: Arc<Node>,
    /// State shared with the session tasks.
    shared: Arc<Shared<P>>,
    // shortcut: one session at a time (Free tier: 2 devices); Pro runs one per peer.
    /// The running attempt or session; a finished handle counts as idle (it ended on its own).
    active: Option<JoinHandle<()>>,
    /// Set by a local Disconnect; only a manual Connect lifts it.
    suppressed: bool,
    /// The peer the running attempt is aimed at; `None` when unknown (an inbound claim, a first pairing).
    aimed: Option<PeerId>,
}

impl<P: Platform + Clone + Sync> Supervisor<P> {
    /// Whether an attempt or session is running.
    fn busy(&self) -> bool {
        self.active.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Dials the paired device that should be reconnected to, if any. Does nothing while a
    /// session runs, the user disconnected or the phase is not idle.
    fn auto_dial(&mut self, my_id: PeerId) {
        let picked = {
            let state = self.shared.state.borrow();
            // Connected again, from either side: the user wants these two together, so a later
            // network drop reconnects even if this device had disconnected earlier.
            if matches!(state.phase, Phase::Connected { .. }) {
                self.suppressed = false;
                return;
            }
            if self.busy() || self.suppressed || !matches!(state.phase, Phase::Idle | Phase::Lost { .. }) {
                return;
            }
            auto_target(
                my_id,
                &state.nearby,
                &self.shared.retry_after.relock(),
                &self.shared.left.relock(),
                Instant::now(),
            )
            .map(|t| (t, state.phase.clone()))
        };
        if let Some((t, restore)) = picked {
            let side = load_side(&self.shared.dir, &t.id);
            self.aimed = Some(t.id);
            let auto = Auto { peer: t.id, restore };
            self.active = Some(dial(&self.node, &self.shared, t.address, side, Some(t.id), Some(auto)));
        }
    }

    /// This computer's addresses changed: old failures say nothing about the new network, so
    /// forget the backoff and look for a device to dial now. A live session is left alone
    /// (`auto_dial` does nothing while connected or busy).
    fn network_changed(&mut self, my_id: PeerId) {
        self.shared.retry_after.relock().clear();
        self.auto_dial(my_id);
    }

    /// Carries out one UI command.
    fn command(&mut self, cmd: Command) {
        let shared = &self.shared;
        match cmd {
            Command::Connect { address, side, expect } if !self.busy() => {
                self.suppressed = false;
                shared.left.relock().clear();
                self.aimed = expect;
                self.active = Some(dial(&self.node, shared, address, side, expect, None));
            }
            // Already in a session: the UI only offers Connect when idle, and
            // replacing a live session's screen with an error would hide it.
            Command::Connect { .. } => {}
            // Arranging more than two screens is Pro; with one peer there is nothing to place.
            Command::Place { .. } | Command::AcceptFiles { .. } | Command::SubmitPin(_) => {}
            Command::AnswerCode(matches) => {
                shared.window.answer(None, matches);
            }
            Command::Answer { code, matches } => {
                // Only the code the user saw: a click on an old prompt is ignored.
                shared.window.answer(Some(code), matches);
            }
            Command::AllowPairing(open) => {
                let until = open.then(|| tokio::time::Instant::now() + PAIRING_WINDOW);
                shared.set_pairing(until);
                if let Some(deadline) = until {
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(deadline).await;
                        // Close only if nobody re-armed it meanwhile.
                        if shared.window.closes_at(deadline) {
                            shared.set_pairing(None);
                        }
                    });
                }
            }
            Command::DismissNotice => {
                shared.state.send_if_modified(|s| s.notice.take().is_some());
            }
            // Only one peer here: naming any other computer is a stale click.
            Command::DisconnectPeer(p) if !matches!(&shared.state.borrow().phase, Phase::Connected { peers, .. } if peers.iter().any(|v| v.id == p)) =>
                {}
            Command::Disconnect | Command::DisconnectPeer(_) => {
                log::info("disconnect requested by the user");
                disconnect(shared, &mut self.active, &mut self.suppressed, Farewell::Leave);
            }
            Command::Forget(peer) => {
                log::info(format_args!("forgetting {}", log::short_id(&peer)));
                let ends = forgetting_ends(&shared.state.borrow().phase, self.busy(), self.aimed, peer);
                // First, so every session that has not published yet is stale, even one mid-handshake.
                shared.unpair(peer);
                if ends {
                    // Forgetting is not "leave me alone": the other paired computers keep reconnecting.
                    let was = self.suppressed;
                    disconnect(shared, &mut self.active, &mut self.suppressed, Farewell::Forget);
                    self.suppressed = was;
                }
            }
        }
    }

    /// Takes an inbound link that passed setup: starts its session, or turns it away if busy.
    fn claim(&mut self, link: Link) {
        if self.busy() {
            link.close(CloseReason::Busy);
            return;
        }
        // Show the attempt right away, so a stalled inbound setup is not invisible.
        self.aimed = None;
        log::info(format_args!("incoming connection from {}", link.remote_address().ip()));
        let generation = self.shared.begin(Phase::Connecting {
            address: link.remote_address(),
        });
        let task_shared = self.shared.clone();
        // The listener's side is decided by the dialer's Desk.
        self.active = Some(spawn_session(&self.shared, generation, async move {
            Dial::new(task_shared, generation, None)
                .run(Ok(link), Side::Right, Direction::Inbound, None)
                .await;
        }));
    }
}

/// The controller's main loop: handles UI commands and incoming connections, and runs
/// at most one session task at a time.
async fn control<P: Platform + Clone + Sync>(
    node: Arc<Node>,
    shared: Arc<Shared<P>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    my_id: PeerId,
) {
    let mut ctl = Supervisor {
        node,
        shared,
        active: None,
        suppressed: false,
        aimed: None,
    };
    // Inbound links that passed setup and the pin or pairing-window check, awaiting a claim.
    let (claims_tx, mut claims) = mpsc::unbounded_channel::<Link>();
    let mut tick = tokio::time::interval(AUTO_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The redial tick runs only while not connected: a connected device has nothing to redial, so
    // it sleeps through a whole session instead of waking every two seconds. Any state change
    // wakes the loop to re-read the phase, which re-arms the tick the moment the session ends
    // (the missed ticks are delayed, not burst, so it fires once at once).
    let mut phase = ctl.shared.state.subscribe();
    loop {
        let connected = matches!(phase.borrow_and_update().phase, Phase::Connected { .. });
        if connected {
            // As `auto_dial` used to find out at a tick: the user wants these two together.
            ctl.suppressed = false;
        }
        tokio::select! {
            _ = tick.tick(), if !connected => ctl.auto_dial(my_id),
            Ok(()) = phase.changed() => {}
            () = ctl.shared.network.notified() => ctl.network_changed(my_id),
            cmd = commands.recv() => match cmd {
                None => break,
                Some(cmd) => ctl.command(cmd),
            },
            Some(setup) = ctl.node.accept() => {
                // Setup and gating run untracked, so strangers, probes and stalled
                // handshakes never make the controller busy.
                let (shared, claims) = (ctl.shared.clone(), claims_tx.clone());
                tokio::spawn(async move {
                    // Inbound setup failures are never published: any LAN host can cause them.
                    let Ok(link) = setup.await else { return };
                    let pinned = shared.pins().contains(&link.peer_fingerprint());
                    if !pinned && !shared.window.allowed() {
                        // Deny by default: no prompt, no state change, nothing to probe.
                        link.close(CloseReason::NotPaired);
                        return;
                    }
                    let _ = claims.send(link);
                });
            }
            Some(link) = claims.recv() => ctl.claim(link),
        }
    }
}

/// Whether forgetting `peer` ends what `phase` shows: its session, or an attempt that may be aimed
/// at it. An attempt aimed at someone else survives; one with no known target (`aimed`) may be this
/// peer. `busy` is whether an attempt or session is running at all.
fn forgetting_ends(phase: &Phase, busy: bool, aimed: Option<PeerId>, peer: PeerId) -> bool {
    match phase {
        Phase::Connected { peers, .. } => peers.iter().any(|v| v.id == peer),
        p => ends_attempt(p) && busy && aimed.is_none_or(|t| t == peer),
    }
}

/// What a peer is told when this device ends its session on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Farewell {
    /// This device just leaves; the peer does not redial.
    Leave,
    /// This device unpaired the peer.
    Forget,
}

/// Ends the running attempt or session on the user's say-so and returns to idle, telling the peer
/// why with `farewell`.
fn disconnect<P: Platform + Clone + Sync>(
    shared: &Shared<P>,
    active: &mut Option<JoinHandle<()>>,
    suppressed: &mut bool,
    farewell: Farewell,
) {
    // Only ending a live attempt means "leave me alone"; dismissing a Lost or
    // Error screen must not switch off auto-reconnect for every paired device.
    if ends_attempt(&shared.state.borrow().phase) {
        *suppressed = true;
        // Tell the peer this is deliberate, so it does not redial us; or that we unpaired it.
        if let Some(closer) = shared.closer.relock().take() {
            match farewell {
                Farewell::Forget => closer.forget(),
                Farewell::Leave => closer.leave(),
            }
        }
    }
    if let Some(task) = active.take() {
        task.abort();
        // An aborted session cannot clean up after itself.
        shared.platform.swallow(false);
        shared.platform.inject(Action::ReleaseAll);
    }
    shared.begin(Phase::Idle);
}

/// Starts a dial in its own task, so a slow peer never blocks Disconnect. `auto` marks
/// an automatic reconnect, whose failures are silent.
fn dial<P: Platform + Clone + Sync>(
    node: &Arc<Node>,
    shared: &Arc<Shared<P>>,
    address: SocketAddr,
    side: Side,
    expect: Option<PeerId>,
    auto: Option<Auto>,
) -> JoinHandle<()> {
    log::info(format_args!(
        "dialing {address} ({})",
        if auto.is_some() { "automatic" } else { "manual" }
    ));
    let generation = shared.begin(Phase::Connecting { address });
    let (node, task_shared) = (node.clone(), shared.clone());
    spawn_session(shared, generation, async move {
        let link = node.connect(address).await;
        Dial::new(task_shared, generation, auto)
            .run(link, side, Direction::Outbound, expect)
            .await;
    })
}

/// Runs a session future under a supervisor: if it panics, the UI gets an error screen
/// instead of staying on the last phase forever. Aborting the returned handle aborts the session.
fn spawn_session<P: Platform + Clone + Sync>(
    shared: &Arc<Shared<P>>,
    generation: u64,
    session: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    let shared = shared.clone();
    tokio::spawn(async move {
        let mut inner = AbortOnDrop(tokio::spawn(session));
        if let Err(e) = (&mut inner.0).await
            && e.is_panic()
        {
            log::warn(format_args!("session task panicked: {e}"));
            shared.set(
                generation,
                Phase::Error("Something went wrong inside Overdesk. Please connect again.".into()),
                Some(Vec::new()),
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Silent;
    use overdesk_core::engine::Mode;
    use overdesk_net::transport::NetError;
    use overdesk_proto::Os;
    use std::sync::atomic::Ordering;

    use super::model::{DeviceKind, PeerView};

    const ME: PeerId = PeerId([1; 16]);
    const OTHER: PeerId = PeerId([2; 16]);

    fn connected_to(id: PeerId) -> Phase {
        Phase::Connected {
            peers: vec![PeerView {
                id,
                name: "peer".into(),
                os: Os::Linux,
                kind: DeviceKind::Computer,
                label: None,
            }],
            mode: Mode::Local,
        }
    }

    fn connecting() -> Phase {
        Phase::Connecting {
            address: "127.0.0.1:1".parse().unwrap(),
        }
    }

    #[test]
    fn forgetting_ends_only_what_may_involve_that_peer() {
        // A session with the peer ends; one with another computer does not, whatever was aimed at.
        assert!(forgetting_ends(&connected_to(ME), true, None, ME));
        assert!(!forgetting_ends(&connected_to(OTHER), true, Some(ME), ME));
        // An attempt aimed at the peer, or at no one known yet, ends; one aimed elsewhere survives.
        assert!(forgetting_ends(&connecting(), true, Some(ME), ME));
        assert!(forgetting_ends(&connecting(), true, None, ME));
        assert!(!forgetting_ends(&connecting(), true, Some(OTHER), ME));
        // Nothing running, or a screen that is not an attempt: nothing to end.
        assert!(!forgetting_ends(&connecting(), false, None, ME));
        assert!(!forgetting_ends(&Phase::Idle, true, None, ME));
        assert!(!forgetting_ends(&Phase::Lost { peer: "x".into() }, true, None, ME));
    }

    /// A [`Shared`] on a fresh folder, and a receiver of what it publishes.
    fn shared(name: &str) -> (Shared<Silent>, watch::Receiver<AppState>) {
        let dir = std::env::temp_dir().join(format!("overdesk-ctl-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).expect("store");
        let (state, rx) = watch::channel(AppState::new("me".into(), Os::Linux));
        let shared = Shared {
            state,
            book: PinBook::new(dir.clone(), Arc::new(store)),
            names: Mutex::new(BTreeMap::new()),
            oses: Mutex::new(BTreeMap::new()),
            dir,
            closer: Mutex::new(None),
            left: Mutex::new(BTreeSet::new()),
            retry_after: Mutex::new(BTreeMap::new()),
            network: Notify::new(),
            window: PairingWindow::default(),
            generation: AtomicU64::new(0),
            save_warned: AtomicBool::new(false),
            vpn_warned: AtomicBool::new(false),
            vpn: || false,
            found: Mutex::new(BTreeMap::new()),
            me: crate::Me {
                name: "me".into(),
                os: Os::Linux,
                width: 1,
                height: 1,
                features: 0,
            },
            admit: None,
            platform: Silent,
        };
        (shared, rx)
    }

    #[tokio::test]
    async fn only_the_newest_generation_may_write_the_phase() {
        let (shared, rx) = shared("generation");
        let old = shared.begin(connecting());
        shared.set(old, Phase::Error("first".into()), None);
        assert_eq!(rx.borrow().phase, Phase::Error("first".into()));
        let new = shared.begin(Phase::Idle);
        assert_ne!(old, new);
        shared.set(old, Phase::Error("stale".into()), Some(Vec::new()));
        assert_eq!(
            rx.borrow().phase,
            Phase::Idle,
            "a session that outlived its abort wrote nothing"
        );
        shared.set(new, connecting(), None);
        assert_eq!(rx.borrow().phase, connecting());
    }

    #[tokio::test]
    async fn disconnecting_means_leave_me_alone_only_for_a_live_attempt() {
        for (phase, ends) in [
            (connecting(), true),
            (connected_to(ME), true),
            (Phase::Idle, false),
            (Phase::Lost { peer: "x".into() }, false),
            (Phase::Error("x".into()), false),
        ] {
            let (shared, rx) = shared("disconnect");
            // With no link to say goodbye on, either farewell must behave the same.
            for farewell in [Farewell::Leave, Farewell::Forget] {
                let generation = shared.begin(phase.clone());
                let mut active = Some(tokio::spawn(std::future::pending::<()>()));
                let mut suppressed = false;
                disconnect(&shared, &mut active, &mut suppressed, farewell);
                assert_eq!(suppressed, ends, "{phase:?} {farewell:?}");
                assert!(active.is_none(), "the task is aborted either way");
                assert_eq!(rx.borrow().phase, Phase::Idle);
                shared.set(generation, connecting(), None);
                assert_eq!(rx.borrow().phase, Phase::Idle, "the old attempt can no longer write");
            }
        }
    }

    /// A supervisor with a paired, nearby `OTHER` (the higher id, so this device dials) that is
    /// backing off for an hour, in `phase`; `name` keeps parallel tests' folders apart.
    fn backing_off(name: &str, phase: Phase) -> (Supervisor<Silent>, watch::Receiver<AppState>) {
        let (shared, rx) = shared(name);
        let peer = NearbyDevice {
            kind: DeviceKind::Computer,
            name: "peer".into(),
            os: Os::Linux,
            address: "127.0.0.1:9".parse().unwrap(),
            id: OTHER,
            paired: true,
        };
        shared.state.send_modify(|s| {
            s.phase = phase;
            s.nearby = vec![peer];
        });
        shared
            .retry_after
            .relock()
            .insert(OTHER, Instant::now() + Duration::from_secs(3600));
        let node = Node::bind(
            &overdesk_net::transport::Identity::generate().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let sup = Supervisor {
            node: Arc::new(node),
            shared: Arc::new(shared),
            active: None,
            suppressed: false,
            aimed: None,
        };
        (sup, rx)
    }

    #[tokio::test]
    async fn a_new_announced_address_ends_that_peers_backoff_only() {
        let (sup, _rx) = backing_off("moved", Phase::Idle);
        let shared = &sup.shared;
        let other = PeerId([8; 16]);
        shared
            .retry_after
            .relock()
            .insert(other, Instant::now() + Duration::from_secs(3600));
        let announce = |id: PeerId, port: u16| {
            Change::Found(overdesk_net::discovery::Nearby {
                id,
                name: "peer".into(),
                os: Os::Linux,
                address: SocketAddr::from(([127, 0, 0, 1], port)),
                pro: false,
            })
        };
        shared.hear(announce(OTHER, 9));
        shared.hear(announce(OTHER, 9));
        assert!(
            shared.retry_after.relock().contains_key(&OTHER),
            "same address: still backing off"
        );
        shared.hear(announce(OTHER, 10));
        let retry = shared.retry_after.relock();
        assert!(!retry.contains_key(&OTHER), "moved: dial the new address at once");
        assert!(retry.contains_key(&other), "another peer's backoff is untouched");
    }

    #[tokio::test]
    async fn a_network_change_ends_the_backoff_and_redials_at_once() {
        let (mut sup, rx) = backing_off("redial", Phase::Idle);
        sup.auto_dial(ME);
        assert!(!sup.busy(), "still backing off");
        sup.network_changed(ME);
        assert!(sup.shared.retry_after.relock().is_empty());
        assert!(sup.busy(), "dialing without waiting for the tick");
        assert!(matches!(rx.borrow().phase, Phase::Connecting { .. }));
    }

    #[tokio::test]
    async fn a_network_change_never_touches_a_live_session() {
        let (mut sup, rx) = backing_off("live", connected_to(OTHER));
        sup.network_changed(ME);
        assert!(!sup.busy());
        assert_eq!(rx.borrow().phase, connected_to(OTHER));
    }

    #[tokio::test]
    async fn a_network_change_does_not_override_a_manual_disconnect() {
        let (mut sup, _rx) = backing_off("manual", Phase::Idle);
        sup.suppressed = true;
        sup.network_changed(ME);
        assert!(!sup.busy());
    }

    /// A failed dial to `OTHER`, automatic or manual, with a VPN up or not.
    async fn failed_dial(shared: &Arc<Shared<Silent>>, auto: bool) {
        let auto = auto.then_some(Auto {
            peer: OTHER,
            restore: Phase::Idle,
        });
        let dial = Dial::new(shared.clone(), shared.begin(Phase::Idle), auto);
        dial.run(
            Err(NetError::Io("down".into())),
            Side::Right,
            Direction::Outbound,
            Some(OTHER),
        )
        .await;
    }

    #[tokio::test]
    async fn a_vpn_hint_is_shown_once_for_automatic_dials_until_a_session_connects() {
        let (mut sh, rx) = shared("vpn-auto");
        sh.vpn = || true;
        let sh = Arc::new(sh);
        failed_dial(&sh, true).await;
        assert_eq!(rx.borrow().notice.as_deref(), Some(VPN_HINT));
        sh.state.send_modify(|s| s.notice = None);
        failed_dial(&sh, true).await;
        assert_eq!(rx.borrow().notice, None, "not again after the user dismissed it");
        sh.vpn_warned.store(false, Ordering::SeqCst); // what a connected session does
        failed_dial(&sh, true).await;
        assert_eq!(rx.borrow().notice.as_deref(), Some(VPN_HINT));
    }

    #[tokio::test]
    async fn a_vpn_hint_ends_a_manual_error_and_never_shows_without_a_vpn() {
        let (mut sh, rx) = shared("vpn-manual");
        sh.vpn = || true;
        let sh = Arc::new(sh);
        failed_dial(&sh, false).await;
        assert!(matches!(&rx.borrow().phase, Phase::Error(m) if m.ends_with(VPN_HINT)));
        assert_eq!(rx.borrow().notice, None);
        let (sh, rx) = shared("no-vpn");
        let sh = Arc::new(sh);
        failed_dial(&sh, true).await;
        failed_dial(&sh, false).await;
        assert_eq!(rx.borrow().notice, None);
        assert!(matches!(&rx.borrow().phase, Phase::Error(m) if !m.contains("VPN")));
    }

    #[tokio::test]
    async fn a_failed_save_tells_the_user_once() {
        let (shared, rx) = shared("saves");
        let failure = || Err(NetError::Io("disk full".into()));
        shared.saved(Ok(()));
        assert_eq!(rx.borrow().notice, None);
        shared.saved(failure());
        let first = rx.borrow().notice.clone();
        assert!(first.is_some());
        shared.state.send_modify(|s| s.notice = None);
        shared.saved(failure());
        assert_eq!(rx.borrow().notice, None, "not again after the user dismissed it");
    }
}
