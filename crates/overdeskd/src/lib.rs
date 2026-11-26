// SPDX-License-Identifier: AGPL-3.0-only
//! The Overdesk daemon: wires capture, the network and injection to the Desk engine.
//! Platform-free; `main.rs` supplies the OS adapter.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
pub mod controller;
#[cfg(any(target_os = "macos", windows, target_os = "linux"))]
pub mod native;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
    /// The clipboard's current text, or `None` if it is empty, holds something else or
    /// cannot be read. May block; `run` calls it off the async threads. The default
    /// (tests, headless) never touches a real clipboard.
    fn clipboard_text(&self) -> Option<String> {
        None
    }
    /// Replaces the clipboard's contents with `text`. May block; same default as above.
    fn set_clipboard_text(&self, _text: &str) {}
}

/// How often the local clipboard is checked for changes.
// shortcut: polling, a 500 ms delay and a cheap read per tick; upgrade to NSPasteboard
// changeCount / AddClipboardFormatListener when the idle wake-ups or the delay matter.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);

/// Room kept below `MAX_FRAME_LEN` for the message tag and the text's length prefix.
const CLIPBOARD_FRAME_OVERHEAD: usize = 16;

/// Decides which local clipboard changes go to the peer.
/// The free tier shares plain text only; images and files are Pro.
struct ClipboardSync {
    /// The text seen on the local clipboard at the last check.
    seen: Option<String>,
    /// The text the peer sent that has not yet been replaced locally; never echoed back.
    remote: Option<String>,
}

impl ClipboardSync {
    /// Starts from `initial`, the clipboard as it was before the session: whatever was
    /// copied earlier is private until the user copies something during the session.
    fn new(initial: Option<String>) -> Self {
        Self {
            seen: initial,
            remote: None,
        }
    }

    /// Feeds one local reading; returns the text to send if it is a new, sendable copy.
    fn local_changed(&mut self, text: Option<String>) -> Option<String> {
        if text == self.seen {
            return None;
        }
        self.seen = text.clone();
        let echo = self.remote.take() == text;
        text.filter(|t| !echo && !t.is_empty() && t.len() + CLIPBOARD_FRAME_OVERHEAD <= overdesk_proto::MAX_FRAME_LEN)
    }

    /// Records text the peer sent, so the change it causes here is not sent back.
    fn received(&mut self, text: &str) {
        self.remote = Some(text.to_owned());
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

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The peer disconnected on purpose; do not dial it again until it reappears.
    PeerLeft,
    /// The link was lost (network, sleep, crash); reconnecting is worthwhile.
    Lost,
}

/// Runs a session until the peer is lost. Leaves the platform safe: not swallowing,
/// nothing held. `on_mode` hears every cursor-mode change. Returns why it ended.
pub async fn run(
    session: Session,
    mut local: UnboundedReceiver<Captured>,
    platform: impl Platform + Sync,
    mut on_mode: impl FnMut(Mode) + Send,
) -> Ended {
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
    let left = Arc::new(AtomicBool::new(false));
    let left_in_reader = left.clone();
    let (control_ev, motion_ev, local_ev) = (events.clone(), events.clone(), events);
    let _tasks = Tasks(vec![
        tokio::spawn(async move {
            loop {
                let event = match rx.recv_control().await {
                    Ok(msg) => Event::Peer { from: peer, msg },
                    Err(e) => {
                        left_in_reader.store(matches!(e, NetError::PeerLeft), Ordering::SeqCst);
                        Event::PeerLost(peer)
                    }
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
    // Clipboard calls can block (the OS may hold the clipboard, or the text is large), so
    // they run on the blocking pool, and polling has its own task: a slow read never
    // delays injecting input.
    let platform = Arc::new(platform);
    let read = |p: Arc<_>| async move {
        tokio::task::spawn_blocking(move || Platform::clipboard_text(&*p))
            .await
            .unwrap_or(None)
    };
    let mut clip = ClipboardSync::new(read(platform.clone()).await);
    let (readings_tx, mut readings) = unbounded_channel::<Option<String>>();
    let poller = platform.clone();
    let _poll = Tasks(vec![tokio::spawn(async move {
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + CLIPBOARD_POLL, CLIPBOARD_POLL);
        loop {
            tick.tick().await;
            if readings_tx.send(read(poller.clone()).await).is_err() {
                break;
            }
        }
    })]);
    let epoch = Instant::now();
    let debug = std::env::var_os("OVERDESK_DEBUG").is_some();
    loop {
        let event = tokio::select! {
            event = queue.recv() => match event {
                Some(event) => event,
                None => break,
            },
            Some(now) = readings.recv() => {
                if let Some(text) = clip.local_changed(now) {
                    // A failed send means the link is going down; the reader reports the loss.
                    let _ = tx.send_control(&Control::Clipboard { text }).await;
                }
                continue;
            }
        };
        // The engine has no use for clipboard text, so it is handled here.
        if let Event::Peer {
            msg: Control::Clipboard { text },
            ..
        } = &event
        {
            clip.received(text);
            let (p, text) = (platform.clone(), text.clone());
            // Not awaited: a slow write must not hold up input either.
            drop(tokio::task::spawn_blocking(move || p.set_clipboard_text(&text)));
            continue;
        }
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
                    if let Err(NetError::Closed(_) | NetError::PeerLeft) = tx.send_control(&msg).await {
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
    if left.load(Ordering::SeqCst) {
        Ended::PeerLeft
    } else {
        Ended::Lost
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn some(s: &str) -> Option<String> {
        Some(s.into())
    }

    #[test]
    fn existing_clipboard_is_not_sent_but_later_copies_are() {
        let mut c = ClipboardSync::new(some("old"));
        assert_eq!(c.local_changed(some("old")), None);
        assert_eq!(c.local_changed(some("new")), some("new"));
        assert_eq!(c.local_changed(some("new")), None, "unchanged text is sent once");
    }

    #[test]
    fn peer_text_is_not_echoed_but_a_later_copy_of_it_is() {
        let mut c = ClipboardSync::new(None);
        c.received("from peer");
        assert_eq!(c.local_changed(some("from peer")), None);
        assert_eq!(c.local_changed(some("mine")), some("mine"));
        assert_eq!(c.local_changed(some("from peer")), some("from peer"));
    }

    #[test]
    fn failed_write_does_not_send_the_old_text() {
        let mut c = ClipboardSync::new(some("old"));
        c.received("from peer");
        assert_eq!(c.local_changed(some("old")), None);
    }

    #[test]
    fn empty_and_oversized_text_is_skipped() {
        let mut c = ClipboardSync::new(None);
        assert_eq!(c.local_changed(some("")), None);
        assert_eq!(c.local_changed(None), None);
        let max = overdesk_proto::MAX_FRAME_LEN - CLIPBOARD_FRAME_OVERHEAD;
        assert_eq!(c.local_changed(Some("x".repeat(max + 1))), None);
        assert!(c.local_changed(Some("y".repeat(max))).is_some());
    }
}
