// SPDX-License-Identifier: AGPL-3.0-only
//! The Overdesk daemon: wires capture, the network and injection to the Desk engine.
//! Platform-free; `main.rs` supplies the OS adapter.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
pub mod controller;
#[cfg(any(target_os = "macos", windows))]
pub mod native;

use std::collections::BTreeMap;
use std::time::Instant;

use overdesk_core::adopt;
use overdesk_core::engine::{Action, Engine, Event, Mode};
use overdesk_net::pairing::{Fingerprint, peer_id};
use overdesk_net::session::{Me, SessionError, hello};
use overdesk_net::transport::{Link, LinkRx, LinkTx, NetError, Role};
use overdesk_platform::input::Captured;
use overdesk_proto::{Control, Desk, DeskScreen, Os, PeerId};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;

/// Why a connection could not become a running session.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The secure link, the trust check or the `Hello` exchange failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The Desk the dialer sent was missing, malformed or could not be adopted.
    #[error("peer did not send a valid Desk: {0}")]
    BadDesk(String),
}

/// Where the peer's screen sits relative to ours (the dialer decides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The peer's screen sits to the left of this device's screen.
    Left,
    /// The peer's screen sits to the right of this device's screen.
    Right,
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
    /// The shared Desk both devices agreed on for this session.
    pub desk: Desk,
}

/// What the OS adapter must do for the engine.
pub trait Platform: Send + 'static {
    /// Turns swallowing on or off: while on, local input drives the peer and must not
    /// also reach this device.
    fn swallow(&self, on: bool);
    /// Performs `Inject*` and `ReleaseAll` actions.
    fn inject(&self, action: Action);
    /// Local input for the next session. Platforms without capture (tests, headless)
    /// return a stream that ends immediately.
    fn capture(&self) -> UnboundedReceiver<Captured> {
        unbounded_channel().1
    }
}

/// Trusts a pinned peer, exchanges `Hello`, agrees on one Desk (the dialer authors it,
/// the listener validates it with `adopt`), and splits the link.
pub async fn establish(
    mut link: Link,
    pinned: impl Fn(&Fingerprint) -> bool,
    me: &Me,
    side: Side,
) -> Result<Session, DaemonError> {
    link.trust(pinned).map_err(SessionError::from)?;
    let Control::Hello {
        peer,
        name,
        os,
        width,
        height,
        ..
    } = hello(&mut link, me).await?
    else {
        unreachable!("hello() only returns Hello")
    };
    let mine = peer_id(&link.local_fingerprint());
    // Screen names come from ids: two Macs both called "MacBook" must not collide.
    let screen = |p: PeerId, x: i32, w: u32, h: u32| DeskScreen {
        peer: p,
        name: hex(&p),
        x,
        y: 0,
        w,
        h,
    };
    let desk = match link.role() {
        Role::Dialer => {
            // The dialer's screen sits at x = 0; the peer's goes beside it on the chosen side.
            let peer_x = match side {
                Side::Right => me.width as i32,
                Side::Left => -(width as i32),
            };
            let desk = Desk {
                version: 1,
                author: mine,
                screens: vec![
                    screen(mine, 0, me.width, me.height),
                    screen(peer, peer_x, width, height),
                ],
            };
            link.send_control(&Control::Desk(desk.clone()))
                .await
                .map_err(SessionError::from)?;
            desk
        }
        Role::Listener => {
            let Control::Desk(incoming) = link.recv_control().await.map_err(SessionError::from)? else {
                return Err(DaemonError::BadDesk("expected Desk".into()));
            };
            // Nothing has been adopted yet, so any valid Desk from the dialer counts as newer.
            let empty = Desk {
                version: 0,
                author: PeerId([0; 16]),
                screens: vec![],
            };
            // Only this device and the introduced peer may own a screen in the incoming Desk.
            adopt(&empty, incoming, peer, |p| *p == mine || *p == peer)
                .map_err(|e| DaemonError::BadDesk(format!("{e:?}")))?
                .ok_or_else(|| DaemonError::BadDesk("not newer".into()))?
        }
    };
    let engine = Engine::new(mine, &desk, BTreeMap::from([(mine, me.os), (peer, os)]))
        .ok_or_else(|| DaemonError::BadDesk("no screen for this device".into()))?;
    let (tx, rx) = link.split().map_err(SessionError::from)?;
    Ok(Session {
        engine,
        tx,
        rx,
        me: mine,
        peer,
        peer_name: name,
        peer_os: os,
        desk,
    })
}

/// Formats a peer id as lowercase hex, used as a stable, collision-free screen name.
fn hex(p: &PeerId) -> String {
    p.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Aborts helper tasks when the session ends or its future is dropped, so nothing
/// keeps the connection (or a stale reader) alive behind our back.
struct Tasks(Vec<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        self.0.iter().for_each(JoinHandle::abort);
    }
}

/// Runs a session until the peer is lost. Leaves the platform safe: not swallowing,
/// nothing held. `on_mode` hears every cursor-mode change.
pub async fn run(
    session: Session,
    mut local: UnboundedReceiver<Captured>,
    platform: impl Platform,
    mut on_mode: impl FnMut(Mode) + Send,
) {
    let Session {
        mut engine,
        mut tx,
        mut rx,
        peer,
        ..
    } = session;
    // Every source feeds one queue, so the engine sees all events in a single order.
    let (events, mut queue) = unbounded_channel::<Event>();
    let motion = rx.motion();
    let (control_ev, motion_ev, local_ev) = (events.clone(), events.clone(), events);
    let _tasks = Tasks(vec![
        tokio::spawn(async move {
            loop {
                let event = match rx.recv_control().await {
                    Ok(msg) => Event::Peer { from: peer, msg },
                    Err(_) => Event::PeerLost(peer),
                };
                // After a loss there is nothing more to read; the engine needs `PeerLost` only once.
                let lost = matches!(event, Event::PeerLost(_));
                if control_ev.send(event).is_err() || lost {
                    break;
                }
            }
        }),
        tokio::spawn(async move {
            while let Ok(m) = motion.recv().await {
                if motion_ev.send(Event::PeerMotion { from: peer, motion: m }).is_err() {
                    break;
                }
            }
        }),
        tokio::spawn(async move {
            while let Some(c) = local.recv().await {
                if local_ev.send(to_event(c)).is_err() {
                    break;
                }
            }
        }),
    ]);
    let epoch = Instant::now();
    let debug = std::env::var_os("OVERDESK_DEBUG").is_some();
    while let Some(event) = queue.recv().await {
        // Note the loss now: the engine consumes the event, and its cleanup actions must
        // still run before the loop stops.
        let lost = event == Event::PeerLost(peer);
        if debug {
            eprintln!("[debug] {:?} <- {event:?}", engine.mode());
        }
        let before = engine.mode();
        let actions = engine.handle(event, epoch.elapsed().as_millis() as u64);
        if engine.mode() != before {
            on_mode(engine.mode());
        }
        if debug && !actions.is_empty() {
            eprintln!("[debug]   -> {actions:?}");
        }
        for action in actions {
            match action {
                // shortcut: one peer per session (Free tier: 2 devices); route by `to` for Pro.
                Action::Send { msg, .. } => {
                    if let Err(NetError::Closed(_)) = tx.send_control(&msg).await {
                        break;
                    }
                }
                Action::SendMotion { motion, .. } => {
                    // Motion travels as datagrams and is best-effort: the next sample supersedes a lost one.
                    let _ = tx.send_motion(&motion);
                }
                Action::Swallow(on) => platform.swallow(on),
                other => platform.inject(other),
            }
        }
        if lost {
            break;
        }
    }
    // Whatever happened, leave this device usable.
    platform.swallow(false);
    platform.inject(Action::ReleaseAll);
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
