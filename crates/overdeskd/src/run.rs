// SPDX-License-Identifier: AGPL-3.0-only
//! Running a session: the loop that feeds local input, the peer's messages and the clipboard to
//! the Desk engine and carries out what it decides.

use std::sync::Arc;
use std::time::{Duration, Instant};

use overdesk_core::engine::{Action, Event, Mode};
use overdesk_net::transport::{LinkRx, LinkTx, MotionRx, NetError};
use overdesk_platform::input::Captured;
use overdesk_proto::{Control, PeerId};
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, channel};
use tokio::sync::oneshot;

use crate::clipboard::ClipboardPump;
use crate::redact::{redact_action, redact_event};
use crate::task::AbortOnDrop;
use crate::{Platform, Session};

/// How long a control message may take to leave before the peer is treated as gone.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The peer disconnected on purpose; do not dial it again until it reappears.
    PeerLeft,
    /// The link was lost (network, sleep, crash); reconnecting is worthwhile.
    Lost,
    /// The peer closed the link because it forgot this device. Act on it only if the peer is pinned.
    Forgotten,
}

/// Leaves the platform safe when a session ends however it ends: a normal return, a panic
/// or the future being dropped by an abort. Not swallowing, nothing held.
struct Cleanup<P: Platform + Sync>(Arc<P>);

impl<P: Platform + Sync> Drop for Cleanup<P> {
    fn drop(&mut self) {
        self.0.swallow(false);
        self.0.inject(Action::ReleaseAll);
    }
}

/// Runs a session until the peer is lost. Leaves the platform safe: not swallowing,
/// nothing held (also if the session panics or is aborted). `on_mode` hears every
/// cursor-mode change. Returns why it ended.
pub async fn run(
    session: Session,
    local: UnboundedReceiver<Captured>,
    platform: impl Platform + Sync,
    on_mode: impl FnMut(Mode) + Send,
) -> Ended {
    run_with(session, local, platform, on_mode, |_| {}).await
}
/// Reads the peer's control messages into `events` until the link is lost, telling `why` how it
/// ended. A frame read must never be cancelled mid-way, hence its own task.
/// If the queue is full the peer is flooding faster than the session can act: reading stops and
/// the session ends, which closes the link.
async fn read_control(mut rx: LinkRx, peer: PeerId, events: Sender<Event>, why: oneshot::Sender<Ended>) {
    let mut why = Some(why);
    loop {
        let event = match rx.recv_control().await {
            Ok(msg) => Event::Peer { from: peer, msg },
            Err(e) => {
                if let Some(why) = why.take() {
                    let _ = why.send(ended_by(&e));
                }
                Event::PeerLost(peer)
            }
        };
        // After a loss there is nothing more to read; the engine needs `PeerLost` only once.
        let lost = matches!(event, Event::PeerLost(_));
        if events.try_send(event).is_err() || lost {
            break;
        }
    }
}

/// How a session whose link failed with `e` ended.
fn ended_by(e: &NetError) -> Ended {
    match e {
        NetError::PeerLeft => Ended::PeerLeft,
        NetError::Forgotten => Ended::Forgotten,
        _ => Ended::Lost,
    }
}

/// How many peer events may wait for the session loop before the peer counts as flooding.
const EVENT_QUEUE: usize = 1024;
/// The next local input. A run of moves already queued is merged into one (deltas added,
/// position of the last) so a burst costs the engine one wake-up; the event that ends the run
/// is parked in `carry` and comes out next, so order is kept. A move that turns back against the
/// sum so far also ends the run: merged, "+60 into the edge, then -10" would hide the crossing.
async fn next_local(local: &mut UnboundedReceiver<Captured>, carry: &mut Option<Captured>) -> Option<Captured> {
    if let Some(c) = carry.take() {
        return Some(c);
    }
    let mut c = local.recv().await?;
    if let Captured::Moved { dx, dy, x, y } = &mut c {
        while let Ok(next) = local.try_recv() {
            match next {
                Captured::Moved {
                    dx: ndx,
                    dy: ndy,
                    x: nx,
                    y: ny,
                } if dx.signum() * ndx.signum() >= 0 && dy.signum() * ndy.signum() >= 0 => {
                    (*dx, *dy, *x, *y) = (dx.saturating_add(ndx), dy.saturating_add(ndy), nx, ny);
                }
                other => {
                    *carry = Some(other);
                    break;
                }
            }
        }
    }
    Some(c)
}

/// Performs the engine's `actions` in order. A failed send does not stop the rest: the Swallow
/// and Inject actions still to run are what hand input back to this device. Returns whether
/// the link is down.
async fn perform<P: Platform + Sync>(actions: &mut Vec<Action>, tx: &mut LinkTx, platform: &P) -> bool {
    let mut link_down = false;
    for action in actions.drain(..) {
        match action {
            // shortcut: one peer per session (Free tier: 2 devices); route by `to` for Pro.
            Action::Send { msg, .. } => {
                // A peer that stops reading must not freeze the loop: a stalled send closes the link.
                if let Err(NetError::Closed(_) | NetError::PeerLeft | NetError::Forgotten) =
                    tx.send_control_timeout(&msg, SEND_TIMEOUT).await
                {
                    link_down = true;
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
    link_down
}

/// Handles what the session loop must not give the engine: the peer's clipboard text goes to
/// `pump`, and the Pro-only image and file offers are dropped. Anything else is returned.
fn intercept(event: Event, pump: &mut ClipboardPump) -> Option<Event> {
    match event {
        // The engine has no use for clipboard text, so it is handled here.
        Event::Peer {
            msg: Control::Clipboard { text },
            ..
        } => {
            pump.received(text);
            None
        }
        // Image and file offers are Pro-only: Free neither answers nor forwards them, and
        // ignoring them keeps the link and the session intact.
        Event::Peer {
            msg: Control::ClipOffer { .. } | Control::ClipGet { .. } | Control::ClipCancel { .. },
            ..
        } => None,
        event => Some(event),
    }
}

/// What the session loop does after one wait.
enum Step {
    /// Feed this event to the engine.
    Event(Event),
    /// A reading of the local clipboard to consider sending.
    Reading(Option<String>),
    /// Nothing to act on; wait again.
    Skip,
    /// Every source of events is gone.
    Stop,
}

/// What the session loop waits on: local input, the peer's motion datagrams and its queued
/// control messages.
struct Sources {
    /// Local input.
    local: UnboundedReceiver<Captured>,
    /// The event that ended a merged run of moves, to come out next.
    carry: Option<Captured>,
    /// Whether `local` may still deliver.
    local_open: bool,
    /// The peer's motion datagrams.
    motion: MotionRx,
    /// Whether `motion` may still deliver.
    motion_open: bool,
    /// The peer's control messages, queued by [`read_control`].
    queue: Receiver<Event>,
    /// The peer, who sent them.
    peer: PeerId,
}

impl Sources {
    /// Waits for the next thing to act on. Local input comes first: it is the latency-critical
    /// path and the engine decides the rest.
    async fn next(&mut self, clip: &mut ClipboardPump) -> Step {
        tokio::select! {
            biased;
            c = next_local(&mut self.local, &mut self.carry), if self.local_open => match c {
                // Trackpad gestures are Pro-only: Free never forwards them.
                Some(Captured::Gesture { .. }) => Step::Skip,
                Some(c) => Step::Event(to_event(c)),
                None => {
                    self.local_open = false;
                    Step::Skip
                }
            },
            m = self.motion.recv(), if self.motion_open => match m {
                Ok(motion) => Step::Event(Event::PeerMotion { from: self.peer, motion }),
                Err(NetError::Closed(_) | NetError::PeerLeft | NetError::Forgotten) => {
                    self.motion_open = false;
                    Step::Skip
                }
                // A malformed datagram is dropped; the next one supersedes it anyway.
                Err(_) => Step::Skip,
            },
            event = self.queue.recv() => event.map_or(Step::Stop, Step::Event),
            Some(now) = clip.next_reading() => Step::Reading(now),
        }
    }
}

/// Prints `line` to stderr when `on` (`OVERDESK_DEBUG`); it is only built then.
fn trace(on: bool, line: impl FnOnce() -> String) {
    if on {
        eprintln!("[debug] {}", line());
    }
}

/// [`run`], and `on_notice` hears each problem worth telling the user (a copy too large to share).
pub async fn run_with(
    session: Session,
    local: UnboundedReceiver<Captured>,
    platform: impl Platform + Sync,
    mut on_mode: impl FnMut(Mode) + Send,
    mut on_notice: impl FnMut(String) + Send,
) -> Ended {
    let Session {
        mut engine,
        mut tx,
        rx,
        peer,
        ..
    } = session;
    let platform = Arc::new(platform);
    let _cleanup = Cleanup(platform.clone());
    engine.set_monitors(platform.monitors());
    // Control messages are read by their own task and queue here; local input and motion
    // datagrams are cancel-safe and are awaited by the loop directly, with no task or channel
    // hop between them and the engine.
    let (events, queue) = channel::<Event>(EVENT_QUEUE);
    let (why, mut ended) = oneshot::channel();
    let mut sources = Sources {
        local,
        carry: None,
        local_open: true,
        motion: rx.motion(),
        motion_open: true,
        queue,
        peer,
    };
    let _reader = AbortOnDrop(tokio::spawn(read_control(rx, peer, events, why)));
    let mut clip = ClipboardPump::start(platform.clone()).await;
    let epoch = Instant::now();
    let debug = std::env::var_os("OVERDESK_DEBUG").is_some();
    let mut actions: Vec<Action> = Vec::new();
    loop {
        let event = match sources.next(&mut clip).await {
            Step::Event(event) => event,
            Step::Reading(now) => {
                if let Some(text) = clip.on_reading(now, &mut on_notice) {
                    // A failed send means the link is going down; the reader reports the loss.
                    let _ = tx
                        .send_control_timeout(&Control::Clipboard { text }, SEND_TIMEOUT)
                        .await;
                }
                continue;
            }
            Step::Skip => continue,
            Step::Stop => break,
        };
        let Some(event) = intercept(event, &mut clip) else {
            continue;
        };
        // Note the loss now: the engine consumes the event, and its cleanup actions must
        // still run before the loop stops.
        let lost = event == Event::PeerLost(peer);
        trace(debug, || format!("{:?} <- {:?}", engine.mode(), redact_event(&event)));
        let before = engine.mode();
        actions.clear();
        engine.handle_into(event, epoch.elapsed().as_millis() as u64, &mut actions);
        if engine.mode() != before {
            on_mode(engine.mode());
        }
        if !actions.is_empty() {
            trace(debug, || {
                format!("  -> {:?}", actions.iter().map(redact_action).collect::<Vec<_>>())
            });
        }
        // A failed send ends the session after the local actions finish.
        let link_down = perform(&mut actions, &mut tx, &*platform).await;
        if lost || link_down {
            break;
        }
    }
    // `_cleanup` leaves this device usable, whatever happened.
    ended.try_recv().unwrap_or(Ended::Lost)
}

/// Platform capture event to engine event.
pub fn to_event(c: Captured) -> Event {
    match c {
        Captured::Moved { dx, dy, x, y } => Event::LocalMove { dx, dy, x, y },
        Captured::Button { button, pressed } => Event::LocalButton { button, pressed },
        Captured::Key { code, pressed } => Event::LocalKey { code, pressed },
        Captured::Scroll { dx, dy, unit, phase } => Event::LocalScroll { dx, dy, unit, phase },
        Captured::Gesture { gesture, phase } => Event::LocalGesture { gesture, phase },
        Captured::Escape => Event::Escape,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[tokio::test]
    async fn a_run_of_moves_is_one_wake_up_and_order_is_kept() {
        let mv = |dx, dy, x, y| Captured::Moved { dx, dy, x, y };
        let key = Captured::Key { code: 4, pressed: true };
        let (tx, mut rx) = unbounded_channel();
        for c in [mv(1, 2, 10, 20), mv(3, 4, 13, 24), key.clone(), mv(5, 6, 18, 30)] {
            tx.send(c).unwrap();
        }
        drop(tx);
        let mut carry = None;
        assert_eq!(next_local(&mut rx, &mut carry).await, Some(mv(4, 6, 13, 24)));
        assert_eq!(next_local(&mut rx, &mut carry).await, Some(key));
        assert_eq!(next_local(&mut rx, &mut carry).await, Some(mv(5, 6, 18, 30)));
        assert_eq!(next_local(&mut rx, &mut carry).await, None);
    }

    #[tokio::test]
    async fn a_move_that_turns_back_is_not_merged_into_the_run() {
        let mv = |dx, dy, x, y| Captured::Moved { dx, dy, x, y };
        let (tx, mut rx) = unbounded_channel();
        for c in [mv(60, 0, 1439, 5), mv(-10, 0, 1429, 5), mv(-5, 3, 1424, 8)] {
            tx.send(c).unwrap();
        }
        drop(tx);
        let mut carry = None;
        assert_eq!(next_local(&mut rx, &mut carry).await, Some(mv(60, 0, 1439, 5)));
        assert_eq!(next_local(&mut rx, &mut carry).await, Some(mv(-10, 0, 1429, 5)));
        assert_eq!(next_local(&mut rx, &mut carry).await, Some(mv(-5, 3, 1424, 8)));
        assert_eq!(next_local(&mut rx, &mut carry).await, None);
    }
}
