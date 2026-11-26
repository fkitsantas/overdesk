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

use overdesk_proto::{Button, Control, Desk, Motion, Os, PeerId, Phase, ScrollUnit, SeqGate};

use crate::{HeldKeys, Motion as Move, Switcher, desk_layout, remap_key};

/// After the source of input changes, it holds for this long, so two people bumping
/// two mice don't fight over the cursor.
pub const TAKEOVER_HOLD_MS: u64 = 300;
/// Local motion smaller than this (|dx| + |dy|) while driven is jitter, not a takeover.
pub const TAKEOVER_MIN_MOVE: i32 = 3;

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
    /// Current handoff role.
    mode: Mode,
    /// Keys and buttons we pressed on the peer we drive.
    sent_keys: HeldKeys,
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
    /// `None` if the Desk is invalid or this device has no screen on it.
    pub fn new(me: PeerId, desk: &Desk, os: BTreeMap<PeerId, Os>) -> Option<Self> {
        let layout = desk_layout(desk).ok()?;
        let screens: Vec<PeerId> = desk.screens.iter().map(|s| s.peer).collect();
        let start = screens.iter().position(|p| *p == me)?;
        Some(Self {
            me,
            os,
            switcher: Switcher::new(layout, start)?,
            screens,
            mode: Mode::Local,
            sent_keys: HeldKeys::default(),
            sent_buttons: Vec::new(),
            local_buttons: Vec::new(),
            seq: 0,
            gate: SeqGate::default(),
            mode_since_ms: 0,
        })
    }

    /// Current handoff role.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Applies one event at monotonic time `now_ms` (milliseconds, any epoch) and
    /// returns the actions to perform, in order. Events that do not apply in the
    /// current mode are ignored and yield no actions.
    pub fn handle(&mut self, event: Event, now_ms: u64) -> Vec<Action> {
        match self.mode {
            Mode::Local => self.local(event, now_ms),
            Mode::Driving(peer) => self.driving(peer, event, now_ms),
            Mode::Driven(peer) => self.driven(peer, event, now_ms),
        }
    }

    /// Index of this device's screen in the layout.
    fn my_screen(&self) -> usize {
        self.screens.iter().position(|p| *p == self.me).expect("checked in new")
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
        for code in self.sent_keys.release_all() {
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

    /// Records a local hardware button change so focus never leaves mid-drag.
    fn track_local_button(&mut self, button: Button, pressed: bool) {
        // Remove first so a repeated press cannot add a duplicate entry.
        self.local_buttons.retain(|b| *b != button);
        if pressed {
            self.local_buttons.push(button);
        }
    }

    /// Becomes driven by `from`, starting a new focus period at motion sequence `seq`,
    /// and moves the local cursor to the entry point (`x`, `y`).
    fn enter_driven(&mut self, from: PeerId, x: i32, y: i32, seq: u32, now_ms: u64) -> Vec<Action> {
        self.mode = Mode::Driven(from);
        self.mode_since_ms = now_ms;
        // A fresh gate per driver, so a previous driver's sequence numbers cannot interfere.
        self.gate = SeqGate::default();
        self.gate.start_at(seq);
        self.switcher.set_local(self.my_screen(), x, y);
        vec![Action::InjectMove { x, y }]
    }

    /// Handles `event` in [`Mode::Local`]: watches for the cursor leaving this screen and
    /// for a peer handing focus to this device.
    fn local(&mut self, event: Event, now_ms: u64) -> Vec<Action> {
        match event {
            Event::LocalMove { dx, dy, x, y } => {
                let me = self.my_screen();
                self.switcher.set_local(me, x, y);
                // A local drag must not carry the cursor onto another device.
                if !self.local_buttons.is_empty() {
                    return vec![];
                }
                // The OS cursor stops at the screen edge; only the part of the delta
                // pushing outward can carry it onto a neighbour.
                let r = self.switcher.layout().screens()[me].rect;
                let (w, h) = (r.w as i32, r.h as i32);
                let push_x = if (x >= w - 1 && dx > 0) || (x <= 0 && dx < 0) {
                    dx
                } else {
                    0
                };
                let push_y = if (y >= h - 1 && dy > 0) || (y <= 0 && dy < 0) {
                    dy
                } else {
                    0
                };
                if (push_x, push_y) == (0, 0) {
                    return vec![];
                }
                match self.switcher.move_by(push_x, push_y) {
                    Move::Switched { to, x, y, .. } => {
                        let peer = self.screens[to];
                        self.mode = Mode::Driving(peer);
                        self.mode_since_ms = now_ms;
                        let mut out = Vec::new();
                        self.broadcast_focus(peer, x, y, &mut out);
                        out.push(Action::Swallow(true));
                        out
                    }
                    Move::Within { .. } => vec![],
                }
            }
            Event::LocalButton { button, pressed } => {
                self.track_local_button(button, pressed);
                vec![]
            }
            Event::Peer {
                from,
                msg: Control::Focus { peer, x, y, seq },
            } if peer == self.me => self.enter_driven(from, x, y, seq, now_ms),
            _ => vec![],
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
    fn driving(&mut self, peer: PeerId, event: Event, now_ms: u64) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::LocalMove { dx, dy, .. } => {
                // Local cursor is frozen while swallowed: dead-reckon on the peer's screen.
                let held = !self.sent_buttons.is_empty();
                let before = self.switcher.local();
                match self.switcher.move_by(dx, dy) {
                    Move::Switched { .. } if held => {
                        // No screen changes mid-drag: undo and stay on the edge.
                        self.switcher.set_local(before.0, before.1, before.2);
                    }
                    Move::Switched { to, x, y, .. } if self.screens[to] == self.me => {
                        self.leave_driving(peer, &mut out);
                        self.broadcast_focus(self.me, x, y, &mut out);
                        out.push(Action::InjectMove { x, y });
                    }
                    Move::Switched { to, x, y, .. } => {
                        self.release_sent(peer, &mut out);
                        let next = self.screens[to];
                        self.mode = Mode::Driving(next);
                        self.mode_since_ms = now_ms;
                        self.broadcast_focus(next, x, y, &mut out);
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
            Event::LocalKey { code, pressed } => {
                if pressed {
                    self.sent_keys.press(code);
                } else {
                    self.sent_keys.release(code);
                }
                out.push(Action::Send {
                    to: peer,
                    msg: Control::Key { code, pressed },
                });
            }
            Event::LocalButton { button, pressed } => {
                self.sent_buttons.retain(|b| *b != button);
                if pressed {
                    self.sent_buttons.push(button);
                }
                let (_, x, y) = self.switcher.local();
                out.push(Action::Send {
                    to: peer,
                    msg: Control::Button { button, pressed, x, y },
                });
            }
            Event::LocalScroll { dx, dy, unit, phase } => {
                out.push(Action::Send {
                    to: peer,
                    msg: Control::Scroll { dx, dy, unit, phase },
                });
            }
            Event::Escape => {
                self.leave_driving(peer, &mut out);
                // Mid-screen, not wherever the hidden cursor drifted: next to the edge a
                // small push would cross straight back.
                let r = self.switcher.layout().screens()[self.my_screen()].rect;
                let (x, y) = ((r.w / 2) as i32, (r.h / 2) as i32);
                self.switcher.set_local(self.my_screen(), x, y);
                self.broadcast_focus(self.me, x, y, &mut out);
                out.push(Action::InjectMove { x, y });
            }
            Event::PeerLost(lost) if lost == peer => {
                // The peer is gone, so there is nobody to send key-ups to: just forget them.
                self.sent_keys.release_all();
                self.sent_buttons.clear();
                out.push(Action::Swallow(false));
                self.mode = Mode::Local;
            }
            // Another device became the source: our hand left the mouse.
            Event::Peer {
                from,
                msg: Control::Focus { peer: focus, x, y, seq },
            } => {
                self.leave_driving(peer, &mut out);
                if focus == self.me {
                    out.extend(self.enter_driven(from, x, y, seq, now_ms));
                }
            }
            _ => {}
        }
        out
    }

    /// Handles `event` in [`Mode::Driven`]: injects input from `driver` only, and lets
    /// local input take focus back once the takeover hold has passed.
    fn driven(&mut self, driver: PeerId, event: Event, now_ms: u64) -> Vec<Action> {
        // Saturating: a clock that steps backwards must not underflow.
        let held = now_ms.saturating_sub(self.mode_since_ms) < TAKEOVER_HOLD_MS;
        // Local input wins: release everything injected and tell every peer focus is here.
        let takeover = |me: &mut Self, x: i32, y: i32| {
            me.mode = Mode::Local;
            me.switcher.set_local(me.my_screen(), x, y);
            let mut out = vec![Action::ReleaseAll];
            me.broadcast_focus(me.me, x, y, &mut out);
            out
        };
        match event {
            Event::Peer { from, msg } if from == driver => match msg {
                Control::Focus { peer, x, y, seq } if peer == self.me => {
                    self.gate.start_at(seq);
                    self.switcher.set_local(self.my_screen(), x, y);
                    vec![Action::InjectMove { x, y }]
                }
                // The driver moved focus elsewhere; this device is no longer the target.
                Control::Focus { .. } => {
                    self.mode = Mode::Local;
                    vec![Action::ReleaseAll]
                }
                Control::Key { code, pressed } => {
                    // Only the receiver remaps, so a key is translated exactly once. An
                    // unknown OS falls back to Linux, which follows the PC convention.
                    let from_os = self.os.get(&from).copied().unwrap_or(Os::Linux);
                    let to_os = self.os.get(&self.me).copied().unwrap_or(Os::Linux);
                    vec![Action::InjectKey {
                        code: remap_key(code, from_os, to_os),
                        pressed,
                    }]
                }
                Control::Button { button, pressed, x, y } => {
                    self.switcher.set_local(self.my_screen(), x, y);
                    vec![Action::InjectMove { x, y }, Action::InjectButton { button, pressed }]
                }
                Control::Scroll { dx, dy, unit, phase } => vec![Action::InjectScroll { dx, dy, unit, phase }],
                _ => vec![],
            },
            Event::PeerMotion { from, motion } if from == driver && self.gate.accept(motion.seq) => {
                self.switcher.set_local(self.my_screen(), motion.x, motion.y);
                vec![Action::InjectMove {
                    x: motion.x,
                    y: motion.y,
                }]
            }
            Event::PeerLost(lost) if lost == driver => {
                self.mode = Mode::Local;
                vec![Action::ReleaseAll]
            }
            Event::LocalMove { dx, dy, x, y } if !held && dx.abs() + dy.abs() >= TAKEOVER_MIN_MOVE => {
                takeover(self, x, y)
            }
            // Escape is the failsafe, so it takes over even inside the hold.
            Event::LocalKey { pressed: true, .. } | Event::LocalButton { pressed: true, .. } | Event::Escape
                if !held || event == Event::Escape =>
            {
                let (_, x, y) = self.switcher.local();
                takeover(self, x, y)
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overdesk_proto::DeskScreen;

    const MAC: PeerId = PeerId([1; 16]);
    const PC: PeerId = PeerId([2; 16]);
    const OTHER: PeerId = PeerId([3; 16]);

    /// Mac 1440x900 on the left, PC 1920x1080 on the right.
    fn engine(me: PeerId) -> Engine {
        let screen = |peer: PeerId, x, w, h| DeskScreen {
            peer,
            name: format!("{:?}", peer.0[0]),
            x,
            y: 0,
            w,
            h,
        };
        let desk = Desk {
            version: 1,
            author: MAC,
            screens: vec![screen(MAC, 0, 1440, 900), screen(PC, 1440, 1920, 1080)],
        };
        Engine::new(me, &desk, BTreeMap::from([(MAC, Os::MacOs), (PC, Os::Windows)])).unwrap()
    }

    fn focus_of(actions: &[Action]) -> Option<(PeerId, PeerId, i32, i32)> {
        actions.iter().find_map(|a| match a {
            Action::Send {
                to,
                msg: Control::Focus { peer, x, y, .. },
            } => Some((*to, *peer, *x, *y)),
            _ => None,
        })
    }

    /// Mac engine with the cursor pushed across its right edge onto the PC at y = 450.
    fn driving() -> Engine {
        let mut e = engine(MAC);
        e.handle(
            Event::LocalMove {
                dx: 5,
                dy: 0,
                x: 1439,
                y: 450,
            },
            0,
        );
        assert_eq!(e.mode(), Mode::Driving(PC));
        e
    }

    fn driven(now: u64) -> Engine {
        let mut e = engine(PC);
        e.handle(
            Event::Peer {
                from: MAC,
                msg: Control::Focus {
                    peer: PC,
                    x: 0,
                    y: 450,
                    seq: 10,
                },
            },
            now,
        );
        assert_eq!(e.mode(), Mode::Driven(MAC));
        e
    }

    #[test]
    fn rejects_desk_without_this_device() {
        let desk = Desk {
            version: 1,
            author: MAC,
            screens: vec![],
        };
        assert!(Engine::new(MAC, &desk, BTreeMap::new()).is_none());
    }

    #[test]
    fn local_motion_inside_screen_does_nothing() {
        let mut e = engine(MAC);
        assert_eq!(
            e.handle(
                Event::LocalMove {
                    dx: 5,
                    dy: 0,
                    x: 700,
                    y: 450
                },
                0
            ),
            vec![]
        );
        assert_eq!(e.mode(), Mode::Local);
    }

    #[test]
    fn pushing_past_the_edge_starts_driving_the_neighbour() {
        let mut e = engine(MAC);
        let a = e.handle(
            Event::LocalMove {
                dx: 5,
                dy: 0,
                x: 1439,
                y: 450,
            },
            0,
        );
        assert_eq!(focus_of(&a), Some((PC, PC, 4, 540)), "450 of 900 maps to 540 of 1080");
        assert!(a.contains(&Action::Swallow(true)));
    }

    #[test]
    fn never_switches_while_a_local_button_is_held() {
        let mut e = engine(MAC);
        e.handle(
            Event::LocalButton {
                button: Button::Left,
                pressed: true,
            },
            0,
        );
        assert_eq!(
            e.handle(
                Event::LocalMove {
                    dx: 5,
                    dy: 0,
                    x: 1439,
                    y: 450
                },
                0
            ),
            vec![]
        );
        assert_eq!(e.mode(), Mode::Local);
    }

    #[test]
    fn driving_sends_motion_with_rising_seq_and_input_to_the_peer() {
        let mut e = driving();
        let a = e.handle(
            Event::LocalMove {
                dx: 10,
                dy: 1,
                x: 1439,
                y: 450,
            },
            1,
        );
        let b = e.handle(
            Event::LocalMove {
                dx: 10,
                dy: 1,
                x: 1439,
                y: 450,
            },
            2,
        );
        let seq = |v: &[Action]| match v {
            [Action::SendMotion { to: PC, motion }] => (motion.seq, motion.x, motion.y),
            other => panic!("{other:?}"),
        };
        let (s1, x1, y1) = seq(&a);
        let (s2, x2, _) = seq(&b);
        assert_eq!((x1, y1, x2), (14, 541, 24));
        assert!(s2 > s1);
        assert_eq!(
            e.handle(
                Event::LocalKey {
                    code: 0xE3,
                    pressed: true
                },
                3
            ),
            vec![Action::Send {
                to: PC,
                msg: Control::Key {
                    code: 0xE3,
                    pressed: true
                }
            }],
            "sender never remaps; the receiver does, exactly once"
        );
        let click = e.handle(
            Event::LocalButton {
                button: Button::Left,
                pressed: true,
            },
            4,
        );
        assert_eq!(
            click,
            vec![Action::Send {
                to: PC,
                msg: Control::Button {
                    button: Button::Left,
                    pressed: true,
                    x: 24,
                    y: 542
                }
            }]
        );
    }

    #[test]
    fn crossing_back_releases_remote_keys_and_returns_input() {
        let mut e = driving();
        e.handle(
            Event::LocalKey {
                code: 0xE1,
                pressed: true,
            },
            1,
        );
        let a = e.handle(
            Event::LocalMove {
                dx: -20,
                dy: 0,
                x: 1439,
                y: 450,
            },
            2,
        );
        assert!(a.contains(&Action::Send {
            to: PC,
            msg: Control::Key {
                code: 0xE1,
                pressed: false
            }
        }));
        assert_eq!(focus_of(&a).map(|f| f.1), Some(MAC), "PC is told focus left it");
        assert!(a.contains(&Action::Swallow(false)));
        assert_eq!(e.mode(), Mode::Local);
    }

    #[test]
    fn escape_and_peer_loss_both_end_driving_safely() {
        let mut e = driving();
        e.handle(
            Event::LocalKey {
                code: 0x04,
                pressed: true,
            },
            1,
        );
        let a = e.handle(Event::Escape, 2);
        assert!(a.contains(&Action::Swallow(false)));
        assert!(a.contains(&Action::Send {
            to: PC,
            msg: Control::Key {
                code: 0x04,
                pressed: false
            }
        }));
        assert_eq!(e.mode(), Mode::Local);

        assert!(
            a.contains(&Action::InjectMove { x: 720, y: 450 }),
            "escape lands mid-screen, not on the edge"
        );

        let mut e = driving();
        assert!(e.handle(Event::PeerLost(PC), 1).contains(&Action::Swallow(false)));
        assert_eq!(e.mode(), Mode::Local);
    }

    #[test]
    fn peer_taking_over_ends_our_driving() {
        let mut e = driving();
        let a = e.handle(
            Event::Peer {
                from: PC,
                msg: Control::Focus {
                    peer: PC,
                    x: 5,
                    y: 5,
                    seq: 0,
                },
            },
            500,
        );
        assert!(a.contains(&Action::Swallow(false)));
        assert_eq!(e.mode(), Mode::Local);
    }

    #[test]
    fn driven_device_injects_with_one_remap() {
        let mut e = engine(PC);
        let a = e.handle(
            Event::Peer {
                from: MAC,
                msg: Control::Focus {
                    peer: PC,
                    x: 0,
                    y: 450,
                    seq: 10,
                },
            },
            0,
        );
        assert_eq!(a, vec![Action::InjectMove { x: 0, y: 450 }]);
        let k = e.handle(
            Event::Peer {
                from: MAC,
                msg: Control::Key {
                    code: 0xE3,
                    pressed: true,
                },
            },
            1,
        );
        assert_eq!(
            k,
            vec![Action::InjectKey {
                code: 0xE0,
                pressed: true
            }],
            "Mac Cmd lands as Ctrl on Windows"
        );
        let b = e.handle(
            Event::Peer {
                from: MAC,
                msg: Control::Button {
                    button: Button::Left,
                    pressed: true,
                    x: 30,
                    y: 40,
                },
            },
            2,
        );
        assert_eq!(
            b,
            vec![
                Action::InjectMove { x: 30, y: 40 },
                Action::InjectButton {
                    button: Button::Left,
                    pressed: true
                }
            ]
        );
    }

    #[test]
    fn driven_motion_honours_the_focus_period() {
        let mut e = driven(0);
        let m = |seq| Event::PeerMotion {
            from: MAC,
            motion: Motion { seq, x: 7, y: 8 },
        };
        assert_eq!(e.handle(m(9), 1), vec![], "from before this focus period");
        assert_eq!(e.handle(m(10), 1), vec![Action::InjectMove { x: 7, y: 8 }]);
        assert_eq!(e.handle(m(10), 1), vec![], "duplicate");
    }

    #[test]
    fn input_from_a_peer_that_is_not_driving_us_is_ignored() {
        let mut e = driven(0);
        assert_eq!(
            e.handle(
                Event::Peer {
                    from: OTHER,
                    msg: Control::Key { code: 4, pressed: true }
                },
                1
            ),
            vec![]
        );
        let mut idle = engine(PC);
        assert_eq!(
            idle.handle(
                Event::Peer {
                    from: MAC,
                    msg: Control::Key { code: 4, pressed: true }
                },
                1
            ),
            vec![]
        );
        assert_eq!(
            idle.handle(
                Event::PeerMotion {
                    from: MAC,
                    motion: Motion { seq: 1, x: 1, y: 1 }
                },
                1
            ),
            vec![]
        );
    }

    #[test]
    fn touching_the_driven_device_takes_over_after_the_hold() {
        let mut e = driven(0);
        assert_eq!(
            e.handle(Event::LocalKey { code: 4, pressed: true }, 100),
            vec![],
            "inside the 300 ms hold"
        );
        assert_eq!(
            e.handle(
                Event::LocalMove {
                    dx: 1,
                    dy: 0,
                    x: 5,
                    y: 5
                },
                400
            ),
            vec![],
            "jitter"
        );
        let a = e.handle(
            Event::LocalMove {
                dx: 4,
                dy: 0,
                x: 5,
                y: 5,
            },
            400,
        );
        assert!(a.contains(&Action::ReleaseAll));
        assert_eq!(
            focus_of(&a),
            Some((MAC, PC, 5, 5)),
            "the Mac learns the cursor is here now"
        );
        assert_eq!(e.mode(), Mode::Local);
    }

    #[test]
    fn driven_ends_cleanly_when_the_driver_leaves_or_vanishes() {
        let mut e = driven(0);
        let a = e.handle(
            Event::Peer {
                from: MAC,
                msg: Control::Focus {
                    peer: MAC,
                    x: 1439,
                    y: 450,
                    seq: 11,
                },
            },
            5,
        );
        assert_eq!(a, vec![Action::ReleaseAll]);
        assert_eq!(e.mode(), Mode::Local);

        let mut e = driven(0);
        assert_eq!(e.handle(Event::PeerLost(MAC), 5), vec![Action::ReleaseAll]);
        assert_eq!(e.mode(), Mode::Local);
    }
}
