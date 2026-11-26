// SPDX-License-Identifier: AGPL-3.0-only
//! What the app runs: one long-lived task that listens, dials, pairs and runs sessions.
//! The UI sends [`Command`]s and renders the [`AppState`] it publishes; nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overdesk_core::engine::{Action, Mode};
use overdesk_net::discovery::{Advertiser, Browser, Change, Nearby};
use overdesk_net::pairing::{Fingerprint, peer_id};
use overdesk_net::session::{Me, SessionError, pair};
use overdesk_net::store::Store;
use overdesk_net::transport::{Closer, Link, NetError, Node};
use overdesk_proto::{Os, PeerId};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::{DaemonError, Ended, Platform, Session, Side, establish, hex, run};

/// What the controller needs to start: where to keep state, where to listen, and who
/// this device is.
pub struct Config {
    /// The folder that holds this device's identity and its pinned peers.
    pub store_dir: PathBuf,
    /// The address to accept incoming connections on.
    pub listen: SocketAddr,
    /// This device's name, operating system and screen size, sent to every peer.
    pub me: Me,
    /// Whether to announce this device on the local network and list the others.
    pub discovery: bool,
}

/// Where the controller is in its connection lifecycle; the UI picks its screen from this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Not connected and not trying to; ready for a [`Command::Connect`].
    Idle,
    /// Dialing another device.
    Connecting {
        /// The address being dialed.
        address: SocketAddr,
    },
    /// Show `code`; the user answers with [`Command::AnswerCode`].
    Pairing {
        /// The other device, by IP address (it has not introduced itself yet).
        peer: String,
        /// The six-digit code both screens must show.
        code: u32,
    },
    /// A session is running.
    Connected {
        /// The peer's device name.
        peer: String,
        /// The peer's operating system.
        peer_os: Os,
        /// Where the cursor is: on this device, driving the peer, or driven by it.
        mode: Mode,
    },
    /// The session ended because the peer went away; input is back on this device.
    Lost {
        /// The device name of the peer that was lost.
        peer: String,
    },
    /// Connecting, pairing or setting up the Desk failed, with a message for people.
    Error(String),
}

/// One screen of the shared Desk, named for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenView {
    /// The device name shown on the screen.
    pub name: String,
    /// The screen's left edge in Desk coordinates.
    pub x: i32,
    /// The screen's top edge in Desk coordinates.
    pub y: i32,
    /// The screen's width in pixels.
    pub w: u32,
    /// The screen's height in pixels.
    pub h: u32,
    /// Whether this screen belongs to this device.
    pub me: bool,
}

/// Another Overdesk computer seen on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearbyDevice {
    /// The device's name.
    pub name: String,
    /// The device's operating system.
    pub os: Os,
    /// Where to dial it; pass it to [`Command::Connect`].
    ///
    /// Only a hint: it comes from an unauthenticated announcement and is verified by the
    /// handshake when connecting (see `Command::Connect`'s `expect`).
    pub address: SocketAddr,
    /// The id the device announced; pass it as `expect` when connecting to a paired one.
    pub id: PeerId,
    /// Whether a pinned device has this id. Display only: the id is unauthenticated, so
    /// trust comes from the handshake, never from this flag.
    pub paired: bool,
}

/// Everything the UI shows, published by the controller after every change.
/// The UI renders it and holds no connection state of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    /// This device's name.
    pub name: String,
    /// This device's operating system.
    pub os: Os,
    /// Where other devices can reach this one.
    pub address: Option<SocketAddr>,
    /// The current connection lifecycle phase.
    pub phase: Phase,
    /// The screens of the shared Desk; empty unless connected.
    pub desk: Vec<ScreenView>,
    /// Whether unknown devices may currently start pairing with this one.
    pub pairing_open: bool,
    /// Other Overdesk computers on the network, sorted by name.
    pub nearby: Vec<NearbyDevice>,
}

/// What the UI asks the controller to do. Commands are the controller's only input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Dial another device and, once trusted, share a Desk with it.
    Connect {
        /// The other device's address.
        address: SocketAddr,
        /// Where the other device's screen sits relative to this one.
        side: Side,
        /// The device this must turn out to be. The UI sets it when the user picked a
        /// nearby device shown as paired: mDNS is unauthenticated, so the dialed peer's
        /// fingerprint must match this id *and* be pinned, or the link is dropped.
        /// `None` means a first pairing or a typed address; the code check protects those.
        expect: Option<PeerId>,
    },
    /// The user's answer to the pairing code: `true` if both screens match.
    AnswerCode(bool),
    /// End the current session or attempt, or dismiss a result, and return to idle.
    Disconnect,
    /// Opens (for [`PAIRING_WINDOW`]) or closes pairing for devices that dial in.
    AllowPairing(bool),
}

/// How long "Pair a device" stays open for incoming devices.
pub const PAIRING_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Why the controller could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The settings folder could not be opened, or the identity could not be loaded.
    #[error("could not open the settings folder: {0}")]
    Store(String),
    /// The listening socket could not be bound.
    #[error("could not listen for other devices: {0}")]
    Bind(String),
}

/// How often the controller looks for a paired device to reconnect to.
const AUTO_INTERVAL: Duration = Duration::from_secs(2);
/// How long to leave a paired device alone after an ordinary failed automatic dial.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// How long after an automatic dial reached the wrong device (stale or spoofed mDNS).
const RETRY_AFTER_UNEXPECTED: Duration = Duration::from_secs(60);
/// The file in the store folder that remembers which side each peer's screen is on.
const SIDES_FILE: &str = "sides.txt";

/// An automatic dial's context: who it targets and the phase to return to if it fails,
/// so a failed attempt never leaves an error screen.
struct Auto {
    /// The paired device being dialed.
    peer: PeerId,
    /// The phase the user was on before the attempt (`Idle` or `Lost`).
    restore: Phase,
}

/// Everything a session task needs from the controller.
struct Shared<P> {
    /// The store folder, which also holds [`SIDES_FILE`].
    dir: PathBuf,
    /// Closes the running session's connection with a deliberate "bye".
    closer: Mutex<Option<Closer>>,
    /// Peers that disconnected on purpose; not redialed until they leave and reappear
    /// on the network, or the user connects manually.
    left: Mutex<BTreeSet<PeerId>>,
    /// Earliest time an automatic dial to a peer may be retried after a failure.
    retry_after: Mutex<BTreeMap<PeerId, Instant>>,
    /// Publishes [`AppState`] to the UI.
    state: watch::Sender<AppState>,
    /// Where the user's answer to the current pairing code goes.
    answer: Mutex<Option<oneshot::Sender<bool>>>,
    /// Until when unknown devices dialing in may pair. Closed by default.
    pairing_until: Mutex<Option<tokio::time::Instant>>,
    /// Which session may write state. Bumped on Disconnect and whenever a session starts,
    /// so a task that outlives its abort cannot overwrite a newer phase.
    generation: AtomicU64,
    /// Devices heard on the network, by id.
    found: Mutex<BTreeMap<PeerId, Nearby>>,
    /// This device's identity and pinned peers.
    store: Store,
    /// Who this device is, as sent to peers.
    me: Me,
    /// The OS adapter that captures and injects input.
    platform: P,
}

impl<P> Shared<P> {
    /// Whether an unknown device dialing in may start pairing right now.
    fn pairing_allowed(&self) -> bool {
        self.pairing_until
            .lock()
            .expect("pairing lock")
            .is_some_and(|t| tokio::time::Instant::now() < t)
    }

    /// Opens pairing until `until`, or closes it with `None`, and tells the UI.
    fn set_pairing(&self, until: Option<tokio::time::Instant>) {
        *self.pairing_until.lock().expect("pairing lock") = until;
        self.state.send_modify(|s| s.pairing_open = until.is_some());
    }

    /// Recomputes the published `nearby` list from what was heard and what is pinned.
    fn refresh_nearby(&self) {
        let pins = self.store.pins().unwrap_or_default();
        let list = nearby_list(&self.found.lock().expect("found lock"), &pins);
        self.state.send_modify(|s| s.nearby = list);
    }

    /// Publishes a new phase and, if given, a new Desk view, unless `generation` is stale.
    fn set(&self, generation: u64, phase: Phase, desk: Option<Vec<ScreenView>>) {
        self.state.send_modify(|s| {
            // Checked under the state lock, so it is atomic with `begin`.
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            s.phase = phase;
            if let Some(d) = desk {
                s.desk = d;
            }
        });
    }

    /// Invalidates every running session, publishes `phase` with an empty Desk, and
    /// returns the new generation for the session (if any) that follows.
    fn begin(&self, phase: Phase) -> u64 {
        let mut generation = 0;
        self.state.send_modify(|s| {
            generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            s.phase = phase;
            s.desk = Vec::new();
        });
        generation
    }
}

/// Starts the controller. Returns its command sender and live state.
pub async fn spawn<P: Platform + Clone + Sync>(
    config: Config,
    platform: P,
) -> Result<(mpsc::UnboundedSender<Command>, watch::Receiver<AppState>), StartError> {
    let store = Store::open(&config.store_dir).map_err(|e| StartError::Store(e.to_string()))?;
    let identity = store.identity().map_err(|e| StartError::Store(e.to_string()))?;
    let node = Node::bind(&identity, config.listen).map_err(|e| StartError::Bind(e.to_string()))?;
    let (state, state_rx) = watch::channel(AppState {
        name: config.me.name.clone(),
        os: config.me.os,
        address: node.local_addr().ok(),
        phase: Phase::Idle,
        desk: Vec::new(),
        pairing_open: false,
        nearby: Vec::new(),
    });
    let shared = Arc::new(Shared {
        state,
        dir: config.store_dir.clone(),
        closer: Mutex::new(None),
        left: Mutex::new(BTreeSet::new()),
        retry_after: Mutex::new(BTreeMap::new()),
        answer: Mutex::new(None),
        pairing_until: Mutex::new(None),
        generation: AtomicU64::new(0),
        found: Mutex::new(BTreeMap::new()),
        store,
        me: config.me,
        platform,
    });
    // shortcut: a device that cannot use mDNS (no interface) simply has no nearby list.
    let discovery = config.discovery.then(|| {
        let port = node.local_addr().ok()?.port();
        let id = peer_id(&identity.fingerprint());
        let advertiser = Advertiser::start(id, &shared.me.name, shared.me.os, port).ok()?;
        let (browser, changes) = Browser::start(id).ok()?;
        let shared = shared.clone();
        // Ends when the browser drops with the controller and closes the channel.
        std::thread::spawn(move || {
            while let Ok(change) = changes.recv() {
                if let Change::Lost(id) = &change {
                    // Gone from the network: a later reappearance is a fresh start.
                    shared.left.lock().expect("left lock").remove(id);
                }
                apply(&mut shared.found.lock().expect("found lock"), change);
                shared.refresh_nearby();
            }
        });
        Some((advertiser, browser))
    });
    let (commands, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        control(Arc::new(node), shared, rx, peer_id(&identity.fingerprint())).await;
        drop(discovery); // announced and browsing for exactly as long as the controller runs
    });
    Ok((commands, state_rx))
}

/// The controller's main loop: handles UI commands and incoming connections, and runs
/// at most one session task at a time.
async fn control<P: Platform + Clone + Sync>(
    node: Arc<Node>,
    shared: Arc<Shared<P>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    my_id: PeerId,
) {
    // shortcut: one session at a time (Free tier: 2 devices); Pro runs one per peer.
    let mut active: Option<JoinHandle<()>> = None;
    // A finished handle counts as idle: the last session ended on its own.
    let busy = |a: &Option<JoinHandle<()>>| a.as_ref().is_some_and(|h| !h.is_finished());
    // Inbound links that passed setup and the pin or pairing-window check, awaiting a claim.
    let (claims_tx, mut claims) = mpsc::unbounded_channel::<Link>();
    // Set by a local Disconnect; only a manual Connect lifts it.
    let mut suppressed = false;
    let mut tick = tokio::time::interval(AUTO_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let (phase, nearby) = {
                    let s = shared.state.borrow();
                    (s.phase.clone(), s.nearby.clone())
                };
                if !busy(&active) && !suppressed && matches!(phase, Phase::Idle | Phase::Lost { .. }) {
                    let target = auto_target(
                        my_id,
                        &nearby,
                        &shared.retry_after.lock().expect("retry lock"),
                        &shared.left.lock().expect("left lock"),
                        Instant::now(),
                    );
                    if let Some(t) = target {
                        let side = load_side(&shared.dir, &t.id);
                        let auto = Auto { peer: t.id, restore: phase };
                        active = Some(dial(&node, &shared, t.address, side, Some(t.id), Some(auto)));
                    }
                }
            }
            cmd = commands.recv() => match cmd {
                None => break,
                Some(Command::Connect { address, side, expect }) if !busy(&active) => {
                    suppressed = false;
                    shared.left.lock().expect("left lock").clear();
                    active = Some(dial(&node, &shared, address, side, expect, None));
                }
                // Already in a session: the UI only offers Connect when idle, and
                // replacing a live session's screen with an error would hide it.
                Some(Command::Connect { .. }) => {}
                Some(Command::AnswerCode(matches)) => {
                    if let Some(tx) = shared.answer.lock().expect("answer lock").take() {
                        let _ = tx.send(matches);
                    }
                }
                Some(Command::AllowPairing(open)) => {
                    let until = open.then(|| tokio::time::Instant::now() + PAIRING_WINDOW);
                    shared.set_pairing(until);
                    if let Some(deadline) = until {
                        let shared = shared.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep_until(deadline).await;
                            // Close only if nobody re-armed it meanwhile.
                            if *shared.pairing_until.lock().expect("pairing lock") == Some(deadline) {
                                shared.set_pairing(None);
                            }
                        });
                    }
                }
                Some(Command::Disconnect) => {
                    suppressed = true;
                    // Tell the peer this is deliberate, so it does not redial us.
                    if let Some(closer) = shared.closer.lock().expect("closer lock").take() {
                        closer.leave();
                    }
                    if let Some(task) = active.take() {
                        task.abort();
                        // An aborted session cannot clean up after itself.
                        shared.platform.swallow(false);
                        shared.platform.inject(Action::ReleaseAll);
                    }
                    shared.begin(Phase::Idle);
                }
            },
            Some(setup) = node.accept() => {
                // Setup and gating run untracked, so strangers, probes and stalled
                // handshakes never make the controller busy.
                let (shared, claims) = (shared.clone(), claims_tx.clone());
                tokio::spawn(async move {
                    // Inbound setup failures are never published: any LAN host can cause them.
                    let Ok(link) = setup.await else { return };
                    let pinned = shared.store.pins().unwrap_or_default().contains(&link.peer_fingerprint());
                    if !pinned && !shared.pairing_allowed() {
                        // Deny by default: no prompt, no state change, nothing to probe.
                        link.close("not paired");
                        return;
                    }
                    let _ = claims.send(link);
                });
            }
            Some(link) = claims.recv() => {
                if busy(&active) {
                    link.close("busy");
                    continue;
                }
                let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
                let shared = shared.clone();
                // The listener's side is decided by the dialer's Desk.
                active = Some(tokio::spawn(async move {
                    session(shared, generation, Ok(link), Side::Right, true, None, None).await;
                }));
            }
        }
    }
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
    let generation = shared.begin(Phase::Connecting { address });
    let (node, shared) = (node.clone(), shared.clone());
    tokio::spawn(async move {
        let link = node.connect(address).await;
        session(shared, generation, link, side, false, expect, auto).await;
    })
}

/// Picks the paired nearby device to dial automatically: only those with a greater id
/// than `me` (the lower id dials, so two devices never dial each other at once), not
/// in `left` and not backing off. The lowest id wins, so the choice is deterministic.
fn auto_target(
    me: PeerId,
    nearby: &[NearbyDevice],
    retry_after: &BTreeMap<PeerId, Instant>,
    left: &BTreeSet<PeerId>,
    now: Instant,
) -> Option<NearbyDevice> {
    nearby
        .iter()
        .filter(|d| d.paired && d.id > me && !left.contains(&d.id))
        .filter(|d| retry_after.get(&d.id).is_none_or(|t| now >= *t))
        .min_by_key(|d| d.id)
        .cloned()
}

/// The saved side of `peer`'s screen, `Right` if none was saved or the file is unreadable.
fn load_side(dir: &std::path::Path, peer: &PeerId) -> Side {
    let key = hex(peer);
    let text = std::fs::read_to_string(dir.join(SIDES_FILE)).unwrap_or_default();
    let found = text
        .lines()
        .find_map(|l| l.strip_prefix(key.as_str())?.strip_prefix(' '));
    if found == Some("left") { Side::Left } else { Side::Right }
}

/// Remembers `side` for `peer`: rewrites the file via a temp file and rename, so a crash
/// never leaves it half-written. Best effort; losing it only costs the default side.
fn save_side(dir: &std::path::Path, peer: &PeerId, side: Side) {
    let key = hex(peer);
    let path = dir.join(SIDES_FILE);
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut text: String = old
        .lines()
        .filter(|l| !l.starts_with(key.as_str()))
        .map(|l| format!("{l}\n"))
        .collect();
    text += &format!("{key} {}\n", if side == Side::Left { "left" } else { "right" });
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Pairs if needed, establishes the Desk, runs until the peer is lost.
/// `inbound` connections from unknown devices pair only while pairing is open, and
/// all state writes are dropped once `generation` is stale. A dial with `expect` set
/// connects only to a pinned peer with that id and never falls back to pairing.
/// An `auto` dial that fails restores the previous phase and backs off instead of
/// showing an error.
async fn session<P: Platform + Clone + Sync>(
    shared: Arc<Shared<P>>,
    generation: u64,
    link: Result<Link, NetError>,
    side: Side,
    inbound: bool,
    expect: Option<PeerId>,
    auto: Option<Auto>,
) {
    // Reports a failed dial: an error screen, or for an automatic one a quiet retreat.
    let fail = |msg: String, wait: Duration| match &auto {
        None => shared.set(generation, Phase::Error(msg), None),
        Some(a) => {
            shared
                .retry_after
                .lock()
                .expect("retry lock")
                .insert(a.peer, Instant::now() + wait);
            shared.set(generation, a.restore.clone(), None);
        }
    };
    let mut link = match link {
        Ok(l) => l,
        // Only dials this device made are worth an error screen.
        Err(_) if inbound => return,
        Err(e) => return fail(format!("Couldn't connect: {e}"), RETRY_AFTER),
    };
    let peer_fp = link.peer_fingerprint();
    if let Some(id) = expect
        && (peer_id(&peer_fp) != id || !shared.store.pins().unwrap_or_default().contains(&peer_fp))
    {
        // Someone else answered at the announced address: never offer to pair with them.
        link.close("unexpected peer");
        return fail(
            "This isn't the computer you paired with. Pair it again if it was reset.".into(),
            RETRY_AFTER_UNEXPECTED,
        );
    }
    // An unknown device: pairing is the only way in.
    if !shared.store.pins().unwrap_or_default().contains(&peer_fp) {
        if inbound && !shared.pairing_allowed() {
            // Deny by default: no prompt, no state change, nothing for a stranger to probe.
            link.close("not paired");
            return;
        }
        let who = link.remote_address().ip().to_string();
        let confirm = async |code: u32| {
            let (tx, rx) = oneshot::channel();
            *shared.answer.lock().expect("answer lock") = Some(tx);
            shared.set(
                generation,
                Phase::Pairing {
                    peer: who.clone(),
                    code,
                },
                None,
            );
            rx.await.unwrap_or(false)
        };
        match pair(&mut link, confirm).await {
            Ok(fp) => {
                // Close the window only if this pairing used it, not the user's own dial.
                if inbound {
                    shared.set_pairing(None);
                }
                if let Err(e) = shared.store.pin(fp) {
                    return shared.set(
                        generation,
                        Phase::Error(format!("Couldn't save the pairing: {e}")),
                        None,
                    );
                }
                shared.refresh_nearby();
            }
            Err(SessionError::Declined) => {
                return shared.set(
                    generation,
                    Phase::Error("Pairing was cancelled on one of the devices.".into()),
                    None,
                );
            }
            Err(e) => return shared.set(generation, Phase::Error(format!("Pairing failed: {e}")), None),
        }
    }
    // Read the pins again: pairing may have just added this peer.
    let pins = shared.store.pins().unwrap_or_default();
    let s = match establish(link, |fp| pins.contains(fp), &shared.me, side).await {
        Ok(s) => s,
        Err(DaemonError::Session(SessionError::Net(e))) => {
            return fail(format!("Connection dropped: {e}"), RETRY_AFTER);
        }
        Err(e) => return fail(format!("Couldn't set up the Desk: {e}"), RETRY_AFTER),
    };
    *shared.closer.lock().expect("closer lock") = Some(s.tx.closer());
    // Remember which side the peer's screen is on, for automatic reconnects.
    let x = |p: PeerId| s.desk.screens.iter().find(|d| d.peer == p).map(|d| d.x);
    if let (Some(mine), Some(theirs)) = (x(s.me), x(s.peer)) {
        save_side(
            &shared.dir,
            &s.peer,
            if theirs > mine { Side::Right } else { Side::Left },
        );
    }
    let peer_id = s.peer;
    let (peer, peer_os) = (s.peer_name.clone(), s.peer_os);
    shared.set(
        generation,
        Phase::Connected {
            peer: peer.clone(),
            peer_os,
            mode: Mode::Local,
        },
        Some(desk_view(&s, &shared.me.name)),
    );
    let watcher = shared.clone();
    let on_mode = move |mode: Mode| {
        watcher.state.send_modify(|st| {
            if watcher.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            // Mode changes only patch a running session; a later phase must never be overwritten.
            if let Phase::Connected { mode: m, .. } = &mut st.phase {
                *m = mode;
            }
        })
    };
    if run(s, shared.platform.capture(), shared.platform.clone(), on_mode).await == Ended::PeerLeft {
        shared.left.lock().expect("left lock").insert(peer_id);
    }
    shared.set(generation, Phase::Lost { peer }, Some(Vec::new()));
}

/// Turns the session's Desk into screens named for people: this device's name for its
/// own screen and the peer's name for the other.
fn desk_view(s: &Session, my_name: &str) -> Vec<ScreenView> {
    let name = |p: &PeerId| {
        if *p == s.me {
            my_name.to_string()
        } else {
            s.peer_name.clone()
        }
    };
    s.desk
        .screens
        .iter()
        .map(|d| ScreenView {
            name: name(&d.peer),
            x: d.x,
            y: d.y,
            w: d.w,
            h: d.h,
            me: d.peer == s.me,
        })
        .collect()
}

/// Folds one network change into the devices heard so far: a device is kept by id, so
/// re-announcing replaces it, and `Lost` removes it. The address is only a hint, since
/// anyone can announce any id; it is verified against the pin when connecting.
fn apply(found: &mut BTreeMap<PeerId, Nearby>, change: Change) {
    match change {
        Change::Found(n) => {
            found.insert(n.id, n);
        }
        Change::Lost(id) => {
            found.remove(&id);
        }
    }
}

/// The list the UI shows: sorted by name, each device marked paired when a pinned
/// fingerprint starts with its id (a [`PeerId`] is a fingerprint's first 16 bytes).
fn nearby_list(found: &BTreeMap<PeerId, Nearby>, pins: &BTreeSet<Fingerprint>) -> Vec<NearbyDevice> {
    let paired: BTreeSet<PeerId> = pins.iter().map(peer_id).collect();
    let mut list: Vec<_> = found
        .values()
        .map(|n| NearbyDevice {
            name: n.name.clone(),
            os: n.os,
            address: n.address,
            id: n.id,
            paired: paired.contains(&n.id),
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device seen on the network, with a one-byte id and an address derived from it.
    fn seen(n: u8, name: &str) -> Nearby {
        Nearby {
            id: PeerId([n; 16]),
            name: name.into(),
            os: Os::Windows,
            address: SocketAddr::from(([10, 0, 0, n], 24801)),
        }
    }

    /// A nearby device with a one-byte id.
    fn dev(n: u8, paired: bool) -> NearbyDevice {
        NearbyDevice {
            name: format!("D{n}"),
            os: Os::Windows,
            address: SocketAddr::from(([10, 0, 0, n], 24801)),
            id: PeerId([n; 16]),
            paired,
        }
    }

    #[test]
    fn only_the_lower_id_dials_a_paired_device() {
        let (none, left, now) = (BTreeMap::new(), BTreeSet::new(), Instant::now());
        let nearby = [dev(1, true), dev(5, false), dev(7, true), dev(9, true)];
        // Lowest greater paired id; the unpaired 5 and the lower 1 are skipped.
        let t = auto_target(PeerId([3; 16]), &nearby, &none, &left, now);
        assert_eq!(t.map(|d| d.id), Some(PeerId([7; 16])));
        // The higher side never dials.
        assert!(auto_target(PeerId([9; 16]), &nearby, &none, &left, now).is_none());
    }

    #[test]
    fn backoff_and_left_peers_are_skipped() {
        let now = Instant::now();
        let nearby = [dev(7, true), dev(9, true)];
        let me = PeerId([3; 16]);
        let retry = BTreeMap::from([(PeerId([7; 16]), now + Duration::from_secs(5))]);
        let t = auto_target(me, &nearby, &retry, &BTreeSet::new(), now);
        assert_eq!(t.map(|d| d.id), Some(PeerId([9; 16])));
        // Once the backoff has passed it is eligible again.
        let t = auto_target(me, &nearby, &retry, &BTreeSet::new(), now + Duration::from_secs(5));
        assert_eq!(t.map(|d| d.id), Some(PeerId([7; 16])));
        let left = BTreeSet::from([PeerId([7; 16]), PeerId([9; 16])]);
        assert!(auto_target(me, &nearby, &BTreeMap::new(), &left, now).is_none());
    }

    #[test]
    fn sides_round_trip_per_peer() {
        let dir = std::env::temp_dir().join(format!("overdesk-sides-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (PeerId([1; 16]), PeerId([2; 16]));
        assert_eq!(load_side(&dir, &a), Side::Right);
        save_side(&dir, &a, Side::Left);
        save_side(&dir, &b, Side::Right);
        save_side(&dir, &b, Side::Left);
        assert_eq!((load_side(&dir, &a), load_side(&dir, &b)), (Side::Left, Side::Left));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_is_sorted_by_name_and_deduplicated_by_id() {
        let mut found = BTreeMap::new();
        for n in [seen(1, "Zed"), seen(2, "Alpha"), seen(1, "Zed renamed")] {
            apply(&mut found, Change::Found(n));
        }
        let names: Vec<_> = nearby_list(&found, &BTreeSet::new())
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["Alpha", "Zed renamed"]);
    }

    #[test]
    fn lost_removes_the_entry() {
        let mut found = BTreeMap::new();
        apply(&mut found, Change::Found(seen(1, "A")));
        apply(&mut found, Change::Lost(PeerId([1; 16])));
        apply(&mut found, Change::Lost(PeerId([9; 16])));
        assert!(nearby_list(&found, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn paired_means_a_pin_starts_with_the_peer_id() {
        let mut found = BTreeMap::new();
        apply(&mut found, Change::Found(seen(1, "Known")));
        apply(&mut found, Change::Found(seen(2, "Stranger")));
        let mut fp = [0u8; 32];
        fp[..16].fill(1);
        fp[16..].fill(0xff); // the rest of the fingerprint must not matter
        let list = nearby_list(&found, &BTreeSet::from([fp]));
        assert_eq!(
            list.iter().map(|d| (d.name.as_str(), d.paired)).collect::<Vec<_>>(),
            [("Known", true), ("Stranger", false)]
        );
        assert_eq!(list[0].address, SocketAddr::from(([10, 0, 0, 1], 24801)));
    }
}
