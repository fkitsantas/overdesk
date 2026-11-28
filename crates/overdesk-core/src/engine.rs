// SPDX-License-Identifier: AGPL-3.0-only
//! The Desk engine: one I/O-free state machine per device. The daemon feeds it events
//! (local capture, peer messages, connection loss) and performs the actions it returns.
//!
//! ```text
//!  Local ──cursor crosses an edge──▶ Driving(P) ──crosses back / Escape / P lost / P takes over──▶ Local
//!  Local ──Focus{me} from S──▶ Driven(S) ──user touches this device / S moves away / S lost──▶ Local
//! ```
//! Exiting Driving always releases keys held on P and stops swallowing. Exiting Driven
//! always releases everything injected here.

use std::collections::BTreeMap;

use std::collections::BTreeSet;

use overdesk_proto::{Button, Control, Desk, Gesture, Motion, Os, PeerId, Phase, ScrollUnit, SeqGate};

use crate::{LayoutError, Move, Rect, Switcher, desk_layout, exit_push, remap_key, snap_to_monitors};

/// After the source of input changes, it holds for this long, so two people bumping
/// two mice don't fight over the cursor.
pub(crate) const TAKEOVER_HOLD_MS: u64 = 300;
/// Local motion smaller than this (|dx| + |dy|) while driven is jitter, not a takeover.
pub(crate) const TAKEOVER_MIN_MOVE: u64 = 3;

/// Input the daemon feeds into [`Engine::handle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Local motion; `x`, `y` are where the OS cursor is, local to this device's screen.
    LocalMove {
        /// Relative horizontal motion in logical pixels, not limited by the screen edge.
        dx: i32,
        /// Relative vertical motion in logical pixels, not limited by the screen edge.
        dy: i32,
        /// OS cursor x after the move, local to this device's screen.
        x: i32,
        /// OS cursor y after the move, local to this device's screen.
        y: i32,
    },
    /// A button on this device's own pointing hardware changed state.
    LocalButton {
        /// Button that changed state.
        button: Button,
        /// `true` for press, `false` for release.
        pressed: bool,
    },
    /// A key on this device's own keyboard changed state.
    LocalKey {
        /// Usage page selector ORed with the HID usage id; see [`overdesk_proto::PAGE_KEYBOARD`].
        code: u32,
        /// `true` for key-down, `false` for key-up.
        pressed: bool,
    },
    /// Scroll input from this device's own hardware.
    LocalScroll {
        /// Horizontal delta in `unit`s.
        dx: i32,
        /// Vertical delta in `unit`s.
        dy: i32,
        /// Unit of `dx` and `dy`.
        unit: ScrollUnit,
        /// Position of this event within a continuous scroll.
        phase: Phase,
    },
    /// A trackpad gesture from this device's own hardware.
    LocalGesture {
        /// The gesture and its delta.
        gesture: Gesture,
        /// Position of this event within the gesture.
        phase: Phase,
    },
    /// The failsafe chord: input is already back locally; tidy up the rest.
    Escape,
    /// A control-lane message from an authenticated peer.
    Peer {
        /// Authenticated sender.
        from: PeerId,
        /// The decoded message.
        msg: Control,
    },
    /// A motion-lane datagram from an authenticated peer.
    PeerMotion {
        /// Authenticated sender.
        from: PeerId,
        /// The decoded datagram.
        motion: Motion,
    },
    /// The connection to this peer closed or timed out.
    PeerLost(PeerId),
}

/// Side effect the daemon must perform, in order, after [`Engine::handle`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send a message to `to` on the control lane.
    Send {
        /// Recipient peer.
        to: PeerId,
        /// Message to send.
        msg: Control,
    },
    /// Send a datagram to `to` on the motion lane.
    SendMotion {
        /// Recipient peer.
        to: PeerId,
        /// Datagram to send.
        motion: Motion,
    },
    /// Consume local input instead of delivering it to local apps.
    Swallow(bool),
    /// Inject at `x`, `y` local to this device's screen.
    InjectMove {
        /// Target x in logical coordinates local to this device's screen.
        x: i32,
        /// Target y in logical coordinates local to this device's screen.
        y: i32,
    },
    /// Inject a button press or release at the current cursor position.
    InjectButton {
        /// Button to press or release.
        button: Button,
        /// `true` for press, `false` for release.
        pressed: bool,
    },
    /// Inject a key event, already remapped for this device's OS.
    InjectKey {
        /// Usage page selector ORed with the HID usage id.
        code: u32,
        /// `true` for key-down, `false` for key-up.
        pressed: bool,
    },
    /// Type `text`: the sender's own layout already produced it ([`Control::KeyText`]), for
    /// a receiver with no keyboard of its own. Desktop adapters ignore it.
    InjectText {
        /// Validated by [`overdesk_proto::key_text_is_valid`].
        text: String,
    },
    /// Inject a scroll event.
    InjectScroll {
        /// Horizontal delta in `unit`s.
        dx: i32,
        /// Vertical delta in `unit`s.
        dy: i32,
        /// Unit of `dx` and `dy`.
        unit: ScrollUnit,
        /// Position of this event within a continuous scroll.
        phase: Phase,
    },
    /// Inject a trackpad gesture.
    InjectGesture {
        /// The gesture and its delta.
        gesture: Gesture,
        /// Position of this event within the gesture.
        phase: Phase,
    },
    /// Release every key and button injected on this device.
    ReleaseAll,
}

/// Role of this device in the Desk's cursor handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// This device holds focus and its input goes to its own apps.
    Local,
    /// Local input is swallowed and forwarded to the peer, which holds focus.
    Driving(PeerId),
    /// The peer is the source: this device holds focus and injects that peer's input.
    Driven(PeerId),
}

/// The per-device cursor handoff state machine. Pure and I/O-free: time arrives as
/// `now_ms` and side effects leave as [`Action`]s.
pub struct Engine {
    /// This device's identity.
    me: PeerId,
    /// Operating system of every device on the Desk, this one included.
    os: BTreeMap<PeerId, Os>,
    /// Owner of each screen, indexed like the layout's screens.
    screens: Vec<PeerId>,
    /// Cursor position on the shared Desk layout.
    switcher: Switcher,
    /// Index of this device's own screen in the layout, cached by `try_new`/`set_desk`.
    my: usize,
    /// This device's real monitors, local to its screen; the screen itself until
    /// [`Engine::set_monitors`] says it has holes.
    monitors: Vec<Rect>,
    /// Whether `monitors` came from [`Engine::set_monitors`] rather than the default
    /// whole-screen rectangle.
    custom_monitors: bool,
    /// Current handoff role.
    mode: Mode,
    /// Keys and buttons we pressed on the peer we drive.
    sent_keys: BTreeSet<u32>,
    /// Buttons we hold down on the peer we drive, in press order.
    sent_buttons: Vec<Button>,
    /// Buttons held on this device's own hardware.
    local_buttons: Vec<Button>,
    /// Sequence number of the next outgoing motion datagram; wraps freely.
    seq: u32,
    /// Filters incoming motion datagrams from the driving peer to the current focus period.
    gate: SeqGate,
    /// `now_ms` when this device last entered `Driving` or `Driven`; the takeover hold
    /// ([`TAKEOVER_HOLD_MS`]) counts from here.
    mode_since_ms: u64,
}

impl Engine {
    /// `os` maps every device on the Desk (including this one) to its OS.
    /// `None` if the Desk is invalid or this device has no screen on it; see
    /// [`Engine::try_new`] for the reason.
    pub fn new(me: PeerId, desk: &Desk, os: BTreeMap<PeerId, Os>) -> Option<Self> {
        Self::try_new(me, desk, os).ok()
    }

    /// Like [`Engine::new`], but says why the Desk was refused.
    ///
    /// # Errors
    /// A [`LayoutError`] if the Desk is invalid (including two screens for one peer), or
    /// [`LayoutError::Unplaced`] if this device has no screen on it.
    pub fn try_new(me: PeerId, desk: &Desk, os: BTreeMap<PeerId, Os>) -> Result<Self, LayoutError> {
        let layout = desk_layout(desk)?;
        let screens: Vec<PeerId> = desk.screens.iter().map(|s| s.peer).collect();
        let start = screens.iter().position(|p| *p == me).ok_or(LayoutError::Unplaced)?;
        let r = layout.screens()[start].rect;
        Ok(Self {
            me,
            os,
            my: start,
            monitors: vec![Rect::whole(r.w, r.h)],
            custom_monitors: false,
            switcher: Switcher::new(layout, start).ok_or(LayoutError::Unplaced)?,
            screens,
            mode: Mode::Local,
            sent_keys: BTreeSet::new(),
            sent_buttons: Vec::new(),
            local_buttons: Vec::new(),
            seq: 0,
            gate: SeqGate::default(),
            mode_since_ms: 0,
        })
    }

    /// Tells the engine this device's monitors (rectangles local to its screen) when they
    /// do not fill it, so the cursor can leave from, and arrive on, a real monitor; see
    /// [`exit_push`]. An empty list keeps the screen as one monitor.
    ///
    /// Call order: the monitors describe the screen as sized now. [`Engine::set_desk`]
    /// resets them to the whole (new) screen when it resizes this device's screen, so call
    /// `set_monitors` again after such a `set_desk`.
    pub fn set_monitors(&mut self, monitors: Vec<Rect>) {
        if !monitors.is_empty() {
            self.monitors = monitors;
            self.custom_monitors = true;
        }
    }

    /// Swaps the layout and OS map in place, keeping held local buttons and keys,
    /// monitors, the sequence state and the mode, so a drag in progress still cannot
    /// cross an edge. Returns the actions to perform (empty when nothing visible changed).
    ///
    /// * The cursor keeps its screen (by owner) and local position, clamped onto the new
    ///   screen; on this device's own screen it is also snapped to the monitors, and an
    ///   [`Action::InjectMove`] is returned if that moved it.
    /// * If this device's screen changed size, the monitors revert to the whole new screen
    ///   (see [`Engine::set_monitors`]); otherwise they are kept.
    /// * `Driving(p)` / `Driven(p)` stay as they are while `p` still has a screen. If `p`
    ///   was removed, it is handled exactly as `PeerLost(p)` (release, un-swallow, back to
    ///   `Local`), and the cursor returns to the middle of this device's screen.
    ///
    /// # Errors
    /// An invalid Desk, or one without a screen for this device, leaves the engine untouched.
    pub fn set_desk(&mut self, desk: &Desk, os: BTreeMap<PeerId, Os>) -> Result<Vec<Action>, LayoutError> {
        let layout = desk_layout(desk)?;
        let screens: Vec<PeerId> = desk.screens.iter().map(|s| s.peer).collect();
        let me = screens
            .iter()
            .position(|p| *p == self.me)
            .ok_or(LayoutError::Unplaced)?;
        let (old_rect, new_rect) = (
            self.switcher.layout().screens()[self.my].rect,
            layout.screens()[me].rect,
        );
        let mut out = match self.mode {
            Mode::Driving(p) | Mode::Driven(p) if !screens.contains(&p) => self.handle(Event::PeerLost(p), 0),
            _ => vec![],
        };
        let (old, x, y) = self.switcher.local();
        let owner = self.screens[old];
        self.switcher = Switcher::new(layout, me).expect("index checked above");
        self.screens = screens;
        self.my = me;
        self.os = os;
        if (old_rect.w, old_rect.h) != (new_rect.w, new_rect.h) {
            // The old monitors describe a box that no longer exists.
            self.monitors = vec![Rect::whole(new_rect.w, new_rect.h)];
            self.custom_monitors = false;
        }
        if owner != self.me {
            match self.screens.iter().position(|p| *p == owner) {
                Some(i) if matches!(self.mode, Mode::Driving(_)) => self.switcher.set_local(i, x, y),
                _ => {
                    // Back on this screen after its driven peer vanished: mid-screen, as when crossing back.
                    let (x, y) = self.center_snap();
                    out.push(Action::InjectMove { x, y });
                }
            }
        } else {
            let moved = self.place(x, y);
            if moved != (Action::InjectMove { x, y }) {
                out.push(moved);
            }
        }
        Ok(out)
    }

    /// This device's monitors as the engine uses them: rectangles local to its screen, the whole
    /// screen until [`Engine::set_monitors`] says otherwise. They change with `set_desk` when it
    /// resizes this device's screen.
    pub fn monitors(&self) -> &[Rect] {
        &self.monitors
    }

    /// Current handoff role.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Applies one event at monotonic time `now_ms` (milliseconds, any epoch) and
    /// returns the actions to perform, in order. Events that do not apply in the
    /// current mode are ignored and yield no actions. Allocates per call; hot paths use
    /// [`Engine::handle_into`].
    pub fn handle(&mut self, event: Event, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        self.handle_into(event, now_ms, &mut out);
        out
    }

    /// Like [`Engine::handle`], but appends the actions to `out` (not cleared first), so a
    /// caller can reuse one buffer for every event.
    pub fn handle_into(&mut self, event: Event, now_ms: u64, out: &mut Vec<Action>) {
        match self.mode {
            Mode::Local => self.local(event, now_ms, out),
            Mode::Driving(peer) => self.driving(peer, event, now_ms, out),
            Mode::Driven(peer) => self.driven(peer, event, now_ms, out),
        }
    }

    /// Every other device on the Desk.
    fn others(&self) -> impl Iterator<Item = PeerId> + '_ {
        self.os.keys().copied().filter(move |p| *p != self.me)
    }

    /// Tells every other device where the cursor is now. Motion of this focus period
    /// starts at the current seq.
    fn broadcast_focus(&self, peer: PeerId, x: i32, y: i32, out: &mut Vec<Action>) {
        let msg = Control::Focus {
            peer,
            x,
            y,
            seq: self.seq,
        };
        out.extend(self.others().map(|to| Action::Send { to, msg: msg.clone() }));
    }

    /// Releases everything we hold down on `peer`.
    fn release_sent(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        for code in std::mem::take(&mut self.sent_keys) {
            out.push(Action::Send {
                to: peer,
                msg: Control::Key { code, pressed: false },
            });
        }
        let (_, x, y) = self.switcher.local();
        for button in std::mem::take(&mut self.sent_buttons) {
            out.push(Action::Send {
                to: peer,
                msg: Control::Button {
                    button,
                    pressed: false,
                    x,
                    y,
                },
            });
        }
    }

    /// Records a button change in `held`, in press order; a repeated press adds no duplicate.
    fn toggle(held: &mut Vec<Button>, button: Button, pressed: bool) {
        held.retain(|b| *b != button);
        if pressed {
            held.push(button);
        }
    }

    /// Becomes driven by `from`, starting a new focus period at motion sequence `seq`,
    /// and moves the local cursor to the entry point (`x`, `y`).
    fn enter_driven(&mut self, from: PeerId, x: i32, y: i32, seq: u32, now_ms: u64, out: &mut Vec<Action>) {
        self.mode = Mode::Driven(from);
        self.mode_since_ms = now_ms;
        // A fresh gate per driver, so a previous driver's sequence numbers cannot interfere.
        self.gate = SeqGate::default();
        self.gate.start_at(seq);
        out.push(self.place(x, y));
    }

    /// Puts the local cursor at the monitor point nearest `(x, y)` and returns the injection.
    fn place(&mut self, x: i32, y: i32) -> Action {
        let (x, y) = self.snap_place(x, y);
        Action::InjectMove { x, y }
    }

    /// Like [`Engine::place`], returning the snapped point, so a caller can announce
    /// exactly where the cursor landed.
    fn snap_place(&mut self, x: i32, y: i32) -> (i32, i32) {
        let (x, y) = snap_to_monitors(&self.monitors, x, y);
        self.switcher.set_local(self.my, x, y);
        (x, y)
    }

    /// Puts the local cursor mid-screen, snapped onto a monitor, and returns that point.
    fn center_snap(&mut self) -> (i32, i32) {
        let r = self.switcher.layout().screens()[self.my].rect;
        self.snap_place((r.w / 2) as i32, (r.h / 2) as i32)
    }

    /// Handles `event` in [`Mode::Local`]: watches for the cursor leaving this screen and
    /// for a peer handing focus to this device.
    fn local(&mut self, event: Event, now_ms: u64, out: &mut Vec<Action>) {
        match event {
            Event::LocalMove { dx, dy, x, y } => {
                let me = self.my;
                self.switcher.set_local(me, x, y);
                // A local drag must not carry the cursor onto another device.
                if !self.local_buttons.is_empty() {
                    return;
                }
                // The OS cursor stops at a monitor edge; only a push off an edge with no
                // monitor beyond it can carry the cursor onto a neighbour.
                let r = self.switcher.layout().screens()[me].rect;
                let (push_x, push_y) = exit_push(&self.monitors, (r.w, r.h), (x, y), (dx, dy));
                if (push_x, push_y) == (0, 0) {
                    return;
                }
                if let Move::Switched { to, x: nx, y: ny, .. } = self.switcher.move_by(push_x, push_y) {
                    let peer = self.screens[to];
                    if self.os.contains_key(&peer) {
                        self.mode = Mode::Driving(peer);
                        self.mode_since_ms = now_ms;
                        self.broadcast_focus(peer, nx, ny, out);
                        out.push(Action::Swallow(true));
                    } else {
                        // A screen whose device we know nothing about is like an offline
                        // one: stay here instead of swallowing input into nothing.
                        self.switcher.set_local(me, x, y);
                    }
                }
            }
            Event::LocalButton { button, pressed } => Self::toggle(&mut self.local_buttons, button, pressed),
            Event::Peer {
                from,
                msg: Control::Focus { peer, x, y, seq },
            } if peer == self.me => self.enter_driven(from, x, y, seq, now_ms, out),
            _ => {}
        }
    }

    /// Ends driving `peer`: releases what we hold there and returns local input to local apps.
    fn leave_driving(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        self.release_sent(peer, out);
        out.push(Action::Swallow(false));
        self.mode = Mode::Local;
    }

    /// Handles `event` in [`Mode::Driving`]: forwards local input to `peer` and moves
    /// focus when the dead-reckoned cursor crosses onto another screen.
    fn driving(&mut self, peer: PeerId, event: Event, now_ms: u64, out: &mut Vec<Action>) {
        match event {
            Event::LocalMove { dx, dy, .. } => self.driving_move(peer, dx, dy, now_ms, out),
            Event::LocalKey { code, pressed } => self.driving_key(peer, code, pressed, out),
            Event::LocalButton { button, pressed } => self.driving_button(peer, button, pressed, out),
            Event::LocalScroll { dx, dy, unit, phase } => {
                out.push(Self::to_peer(peer, Control::Scroll { dx, dy, unit, phase }))
            }
            Event::LocalGesture { gesture, phase } => {
                out.push(Self::to_peer(peer, Control::Gesture { gesture, phase }))
            }
            Event::Escape => self.driving_escape(peer, out),
            Event::PeerLost(lost) if lost == peer => self.driving_peer_lost(out),
            // Another device became the source: our hand left the mouse.
            Event::Peer {
                from,
                msg: Control::Focus { peer: focus, x, y, seq },
            } => {
                self.leave_driving(peer, out);
                if focus == self.me {
                    self.enter_driven(from, x, y, seq, now_ms, out);
                }
            }
            _ => {}
        }
    }

    /// `msg` for the peer being driven.
    fn to_peer(peer: PeerId, msg: Control) -> Action {
        Action::Send { to: peer, msg }
    }

    /// Local motion while driving `peer`: dead-reckons the cursor on the peer's screen and
    /// either sends the motion, or moves focus when it crosses onto another screen.
    fn driving_move(&mut self, peer: PeerId, dx: i32, dy: i32, now_ms: u64, out: &mut Vec<Action>) {
        // Local cursor is frozen while swallowed: dead-reckon on the peer's screen.
        let held = !self.sent_buttons.is_empty();
        let before = self.switcher.local();
        match self.switcher.move_by(dx, dy) {
            Move::Switched { .. } if held => {
                // No screen changes mid-drag: undo and stay on the edge.
                self.switcher.set_local(before.0, before.1, before.2);
            }
            Move::Switched { to, x, y, .. } if self.screens[to] == self.me => {
                self.leave_driving(peer, out);
                // Announce the point the cursor really lands on, snapped once.
                let (x, y) = self.snap_place(x, y);
                self.broadcast_focus(self.me, x, y, out);
                out.push(Action::InjectMove { x, y });
            }
            Move::Switched { to, .. } if !self.os.contains_key(&self.screens[to]) => {
                // Offline-like screen: stay on the edge rather than drive nobody.
                self.switcher.set_local(before.0, before.1, before.2);
            }
            Move::Switched { to, x, y, .. } => {
                self.release_sent(peer, out);
                let next = self.screens[to];
                self.mode = Mode::Driving(next);
                self.mode_since_ms = now_ms;
                self.broadcast_focus(next, x, y, out);
            }
            Move::Within { x, y, .. } => {
                out.push(Action::SendMotion {
                    to: peer,
                    motion: Motion { seq: self.seq, x, y },
                });
                self.seq = self.seq.wrapping_add(1);
            }
        }
    }

    /// A local key edge while driving `peer`: remembered so it can be released, and sent.
    fn driving_key(&mut self, peer: PeerId, code: u32, pressed: bool, out: &mut Vec<Action>) {
        if pressed {
            self.sent_keys.insert(code);
        } else {
            self.sent_keys.remove(&code);
        }
        out.push(Self::to_peer(peer, Control::Key { code, pressed }));
    }

    /// A local button edge while driving `peer`: remembered so it can be released, and sent
    /// with the dead-reckoned position.
    fn driving_button(&mut self, peer: PeerId, button: Button, pressed: bool, out: &mut Vec<Action>) {
        Self::toggle(&mut self.sent_buttons, button, pressed);
        let (_, x, y) = self.switcher.local();
        out.push(Self::to_peer(peer, Control::Button { button, pressed, x, y }));
    }

    /// The escape chord while driving `peer`: input comes home.
    fn driving_escape(&mut self, peer: PeerId, out: &mut Vec<Action>) {
        self.leave_driving(peer, out);
        // Mid-screen, not wherever the hidden cursor drifted: next to the edge a
        // small push would cross straight back.
        let (x, y) = self.center_snap();
        out.push(Action::InjectMove { x, y });
        self.broadcast_focus(self.me, x, y, out);
    }

    /// The peer being driven is gone: nobody is left to send key-ups to, so just forget them.
    fn driving_peer_lost(&mut self, out: &mut Vec<Action>) {
        self.sent_keys.clear();
        self.sent_buttons.clear();
        out.push(Action::Swallow(false));
        self.mode = Mode::Local;
    }

    /// Handles `event` in [`Mode::Driven`]: injects input from `driver` only, and lets
    /// local input take focus back once the takeover hold has passed.
    fn driven(&mut self, driver: PeerId, event: Event, now_ms: u64, out: &mut Vec<Action>) {
        // Saturating: a clock that steps backwards must not underflow.
        let held = now_ms.saturating_sub(self.mode_since_ms) < TAKEOVER_HOLD_MS;
        // Local input wins: release everything injected and tell every peer focus is here.
        let takeover = |me: &mut Self, x: i32, y: i32, out: &mut Vec<Action>| {
            me.mode = Mode::Local;
            me.switcher.set_local(me.my, x, y);
            out.push(Action::ReleaseAll);
            me.broadcast_focus(me.me, x, y, out);
        };
        match event {
            Event::Peer { from, msg } if from == driver => match msg {
                Control::Focus { peer, x, y, seq } if peer == self.me => {
                    self.gate.start_at(seq);
                    out.push(self.place(x, y));
                }
                // The driver moved focus elsewhere; this device is no longer the target.
                Control::Focus { .. } => {
                    self.mode = Mode::Local;
                    out.push(Action::ReleaseAll);
                }
                Control::Key { code, pressed } => {
                    // Only the receiver remaps, so a key is translated exactly once. An
                    // unknown OS falls back to Linux, which follows the PC convention.
                    let from_os = self.os.get(&from).copied().unwrap_or(Os::Linux);
                    let to_os = self.os.get(&self.me).copied().unwrap_or(Os::Linux);
                    out.push(Action::InjectKey {
                        code: remap_key(code, from_os, to_os),
                        pressed,
                    });
                }
                // The trust boundary: text goes straight into a focused field.
                Control::KeyText { text, .. } if overdesk_proto::key_text_is_valid(&text) => {
                    out.push(Action::InjectText { text });
                }
                Control::Button { button, pressed, x, y } => {
                    out.push(self.place(x, y));
                    out.push(Action::InjectButton { button, pressed });
                }
                Control::Scroll { dx, dy, unit, phase } => out.push(Action::InjectScroll { dx, dy, unit, phase }),
                Control::Gesture { gesture, phase } => out.push(Action::InjectGesture { gesture, phase }),
                _ => {}
            },
            Event::PeerMotion { from, motion } if from == driver && self.gate.accept(motion.seq) => {
                out.push(self.place(motion.x, motion.y));
            }
            Event::PeerLost(lost) if lost == driver => {
                self.mode = Mode::Local;
                out.push(Action::ReleaseAll);
            }
            Event::LocalMove { dx, dy, x, y }
                if !held && u64::from(dx.unsigned_abs()) + u64::from(dy.unsigned_abs()) >= TAKEOVER_MIN_MOVE =>
            {
                takeover(self, x, y, out);
            }
            // Escape is the failsafe, so it takes over even inside the hold.
            Event::LocalKey { pressed: true, .. } | Event::LocalButton { pressed: true, .. } | Event::Escape
                if !held || event == Event::Escape =>
            {
                let (_, x, y) = self.switcher.local();
                takeover(self, x, y, out);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
