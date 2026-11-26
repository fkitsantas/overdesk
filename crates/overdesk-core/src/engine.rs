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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Local motion; `x`, `y` are where the OS cursor is, local to this device's screen.
    LocalMove {
        dx: i32,
        dy: i32,
        x: i32,
        y: i32,
    },
    LocalButton {
        button: Button,
        pressed: bool,
    },
    LocalKey {
        code: u32,
        pressed: bool,
    },
    LocalScroll {
        dx: i32,
        dy: i32,
        unit: ScrollUnit,
        phase: Phase,
    },
    /// The failsafe chord: input is already back locally; tidy up the rest.
    Escape,
    Peer {
        from: PeerId,
        msg: Control,
    },
    PeerMotion {
        from: PeerId,
        motion: Motion,
    },
    PeerLost(PeerId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Send {
        to: PeerId,
        msg: Control,
    },
    SendMotion {
        to: PeerId,
        motion: Motion,
    },
    /// Consume local input instead of delivering it to local apps.
    Swallow(bool),
    /// Inject at `x`, `y` local to this device's screen.
    InjectMove {
        x: i32,
        y: i32,
    },
    InjectButton {
        button: Button,
        pressed: bool,
    },
    InjectKey {
        code: u32,
        pressed: bool,
    },
    InjectScroll {
        dx: i32,
        dy: i32,
        unit: ScrollUnit,
        phase: Phase,
    },
    /// Release every key and button injected on this device.
    ReleaseAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Local,
    Driving(PeerId),
    Driven(PeerId),
}

pub struct Engine {
    me: PeerId,
    os: BTreeMap<PeerId, Os>,
    screens: Vec<PeerId>,
    switcher: Switcher,
    mode: Mode,
    /// Keys and buttons we pressed on the peer we drive.
    sent_keys: HeldKeys,
    sent_buttons: Vec<Button>,
    /// Buttons held on this device's own hardware.
    local_buttons: Vec<Button>,
    seq: u32,
    gate: SeqGate,
    mode_since_ms: u64,
}

impl Engine {
    /// `os` maps every device on the Desk (including this one) to its OS.
    /// `None` if the Desk is invalid or this device has no screen on it.
    pub fn new(_me: PeerId, _desk: &Desk, _os: BTreeMap<PeerId, Os>) -> Option<Self> {
        todo!()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn handle(&mut self, _event: Event, _now_ms: u64) -> Vec<Action> {
        todo!()
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
        assert_eq!(focus_of(&a), Some((PC, PC, 4, 450)));
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
        assert_eq!((x1, y1, x2), (14, 451, 24));
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
                    y: 452
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
