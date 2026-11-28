// SPDX-License-Identifier: AGPL-3.0-only
//! Tests of the Desk engine.

use super::*;
use crate::tests::r;
use overdesk_proto::DeskScreen;

const MAC: PeerId = PeerId([1; 16]);
const PC: PeerId = PeerId([2; 16]);
const OTHER: PeerId = PeerId([3; 16]);

/// A screen of `peer` at (`x`, 0), named after it.
fn ds(peer: PeerId, x: i32, w: u32, h: u32) -> DeskScreen {
    DeskScreen {
        peer,
        name: format!("{:?}", peer.0[0]),
        x,
        y: 0,
        w,
        h,
    }
}

/// A local cursor move by (`dx`, `dy`), with the OS cursor now at (`x`, `y`).
fn mv(dx: i32, dy: i32, x: i32, y: i32) -> Event {
    Event::LocalMove { dx, dy, x, y }
}

/// `from` hands focus to `peer`, entering at (`x`, `y`) with motion sequence `seq`.
fn focus(from: PeerId, peer: PeerId, x: i32, y: i32, seq: u32) -> Event {
    Event::Peer {
        from,
        msg: Control::Focus { peer, x, y, seq },
    }
}

/// A local button press or release.
fn btn(button: Button, pressed: bool) -> Event {
    Event::LocalButton { button, pressed }
}

/// Mac 1440x900 on the left, PC 1920x1080 on the right.
fn engine(me: PeerId) -> Engine {
    let desk = Desk {
        version: 1,
        author: MAC,
        screens: vec![ds(MAC, 0, 1440, 900), ds(PC, 1440, 1920, 1080)],
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
    e.handle(mv(5, 0, 1439, 450), 0);
    assert_eq!(e.mode(), Mode::Driving(PC));
    e
}

fn driven(now: u64) -> Engine {
    let mut e = engine(PC);
    e.handle(focus(MAC, PC, 0, 450, 10), now);
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
    assert_eq!(e.handle(mv(5, 0, 700, 450), 0), vec![]);
    assert_eq!(e.mode(), Mode::Local);
}

#[test]
fn pushing_past_the_edge_starts_driving_the_neighbour() {
    let mut e = engine(MAC);
    let a = e.handle(mv(5, 0, 1439, 450), 0);
    assert_eq!(focus_of(&a), Some((PC, PC, 4, 540)), "450 of 900 maps to 540 of 1080");
    assert!(a.contains(&Action::Swallow(true)));
}

/// The two-screen Desk with a third screen (`OTHER`) to the left of the Mac.
fn desk3(with_pc: bool) -> Desk {
    let mut screens = vec![ds(OTHER, -500, 500, 900), ds(MAC, 0, 1440, 900)];
    if with_pc {
        screens.push(ds(PC, 1440, 1920, 1080));
    }
    Desk {
        version: 2,
        author: MAC,
        screens,
    }
}

fn os3() -> BTreeMap<PeerId, Os> {
    BTreeMap::from([(MAC, Os::MacOs), (PC, Os::Windows), (OTHER, Os::Linux)])
}

#[test]
fn set_desk_keeps_a_drag_in_progress_on_this_ds() {
    let mut e = engine(MAC);
    e.handle(btn(Button::Left, true), 0);
    assert_eq!(e.set_desk(&desk3(true), os3()), Ok(vec![]));
    let push = |dx, x| Event::LocalMove { dx, dy: 0, x, y: 450 };
    assert_eq!(e.handle(push(5, 1439), 1), vec![], "no crossing while dragging");
    assert_eq!(e.handle(push(-5, 0), 2), vec![], "nor on the new left edge");
    assert_eq!(e.mode(), Mode::Local);
    e.handle(btn(Button::Left, false), 3);
    e.handle(push(-5, 0), 4);
    assert_eq!(e.mode(), Mode::Driving(OTHER), "the new layout is live once released");
}

#[test]
fn set_desk_keeps_driving_a_peer_that_stays() {
    let mut e = driving();
    e.handle(Event::LocalKey { code: 4, pressed: true }, 1);
    assert_eq!(e.set_desk(&desk3(true), os3()), Ok(vec![]));
    assert_eq!(e.mode(), Mode::Driving(PC));
    let out = e.handle(Event::Escape, 2);
    assert!(out.iter().any(|a| matches!(
        a,
        Action::Send {
            to: PC,
            msg: Control::Key { pressed: false, .. }
        }
    )));
    assert_eq!(e.mode(), Mode::Local);
}

#[test]
fn set_desk_treats_a_removed_peer_like_peer_lost() {
    let mut e = driving();
    e.handle(Event::LocalKey { code: 4, pressed: true }, 1);
    let out = e.set_desk(&desk3(false), os3()).unwrap();
    assert_eq!(e.mode(), Mode::Local);
    assert!(out.contains(&Action::Swallow(false)));
    assert!(
        out.contains(&Action::InjectMove { x: 720, y: 450 }),
        "cursor returns mid-screen"
    );
    // Nothing is left to release.
    assert_eq!(e.handle(Event::Escape, 3), vec![]);
    // Driven by a removed peer: released like a lost driver.
    let mut d = driven(0);
    let mac_only = Desk {
        screens: vec![desk3(true).screens[2].clone()],
        ..desk3(true)
    };
    let out = d.set_desk(&mac_only, BTreeMap::from([(PC, Os::Windows)])).unwrap();
    assert_eq!(d.mode(), Mode::Local);
    assert_eq!(out.first(), Some(&Action::ReleaseAll));
}

#[test]
fn set_desk_rejects_a_bad_desk_and_changes_nothing() {
    let mut e = driving();
    let no_me = Desk {
        screens: desk3(true).screens[..1].to_vec(),
        ..desk3(true)
    };
    assert_eq!(e.set_desk(&no_me, os3()), Err(LayoutError::Unplaced));
    let empty = Desk {
        screens: vec![],
        ..desk3(true)
    };
    assert_eq!(e.set_desk(&empty, os3()), Err(LayoutError::Empty));
    assert_eq!(e.mode(), Mode::Driving(PC));
}

#[test]
fn never_switches_while_a_local_button_is_held() {
    let mut e = engine(MAC);
    e.handle(btn(Button::Left, true), 0);
    assert_eq!(e.handle(mv(5, 0, 1439, 450), 0), vec![]);
    assert_eq!(e.mode(), Mode::Local);
}

#[test]
fn driving_sends_motion_with_rising_seq_and_input_to_the_peer() {
    let mut e = driving();
    let a = e.handle(mv(10, 1, 1439, 450), 1);
    let b = e.handle(mv(10, 1, 1439, 450), 2);
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
    let click = e.handle(btn(Button::Left, true), 4);
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
    let a = e.handle(mv(-20, 0, 1439, 450), 2);
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
fn leaving_and_arriving_on_an_l_shaped_screen_use_the_real_monitors() {
    // Mac 1440x900 whose laptop panel is low-left and display high-right: hole top-left.
    let m = r;
    let mut e = engine(MAC);
    e.set_monitors(vec![m(0, 100, 700, 800), m(700, 0, 740, 900)]);
    // Pushing up off the panel's top edge leaves the screen, but no device is above.
    assert_eq!(e.handle(mv(0, -5, 300, 100), 1), vec![]);
    // Pushing right off the display (box right edge) drives the PC as before.
    let a = e.handle(mv(5, 0, 1439, 400), 2);
    assert!(matches!(e.mode(), Mode::Driving(p) if p == PC), "{a:?}");
    // A peer entering at the top-left hole lands on the laptop panel.
    let mut e = engine(MAC);
    e.set_monitors(vec![m(0, 100, 700, 800), m(700, 0, 740, 900)]);
    let a = e.handle(focus(PC, MAC, 0, 0, 0), 3);
    assert_eq!(a, vec![Action::InjectMove { x: 0, y: 100 }]);
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
    let a = e.handle(focus(PC, PC, 5, 5, 0), 500);
    assert!(a.contains(&Action::Swallow(false)));
    assert_eq!(e.mode(), Mode::Local);
}

#[test]
fn driven_device_types_valid_key_text_only() {
    let mut e = engine(PC);
    e.handle(focus(MAC, PC, 0, 450, 10), 0);
    let mut text = |t: &str| {
        e.handle(
            Event::Peer {
                from: MAC,
                msg: Control::KeyText {
                    code: 4,
                    text: t.into(),
                },
            },
            1,
        )
    };
    assert_eq!(text("é"), vec![Action::InjectText { text: "é".into() }]);
    assert!(text("").is_empty() && text("\n").is_empty() && text("123456789").is_empty());
    assert!(
        text("\u{202E}").is_empty(),
        "a bidi override from the driver is dropped"
    );
}

#[test]
fn key_text_from_a_non_driver_or_while_local_types_nothing() {
    const THIRD: PeerId = PeerId([9; 16]);
    let key_text = |from| Event::Peer {
        from,
        msg: Control::KeyText {
            code: 4,
            text: "a".into(),
        },
    };
    let mut e = engine(PC);
    assert!(e.handle(key_text(MAC), 0).is_empty(), "Local: nobody is driving");
    e.handle(focus(MAC, PC, 0, 450, 10), 1);
    assert!(e.handle(key_text(THIRD), 2).is_empty(), "a peer that is not the driver");
    assert_eq!(
        e.handle(key_text(MAC), 3),
        vec![Action::InjectText { text: "a".into() }]
    );
}

#[test]
fn driven_device_injects_with_one_remap() {
    let mut e = engine(PC);
    let a = e.handle(focus(MAC, PC, 0, 450, 10), 0);
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
    assert_eq!(e.handle(mv(1, 0, 5, 5), 400), vec![], "jitter");
    let a = e.handle(mv(4, 0, 5, 5), 400);
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
    let a = e.handle(focus(MAC, MAC, 1439, 450, 11), 5);
    assert_eq!(a, vec![Action::ReleaseAll]);
    assert_eq!(e.mode(), Mode::Local);

    let mut e = driven(0);
    assert_eq!(e.handle(Event::PeerLost(MAC), 5), vec![Action::ReleaseAll]);
    assert_eq!(e.mode(), Mode::Local);
}

#[test]
fn gestures_are_sent_when_driving_and_injected_when_driven() {
    let g = Gesture::Magnify { milli: 40 };
    let sent = driving().handle(
        Event::LocalGesture {
            gesture: g,
            phase: Phase::Changed,
        },
        1,
    );
    assert_eq!(
        sent,
        vec![Action::Send {
            to: PC,
            msg: Control::Gesture {
                gesture: g,
                phase: Phase::Changed
            }
        }]
    );
    let got = driven(0).handle(
        Event::Peer {
            from: MAC,
            msg: Control::Gesture {
                gesture: g,
                phase: Phase::Began,
            },
        },
        1,
    );
    assert_eq!(
        got,
        vec![Action::InjectGesture {
            gesture: g,
            phase: Phase::Began
        }]
    );
    let local = engine(MAC).handle(
        Event::LocalGesture {
            gesture: g,
            phase: Phase::Changed,
        },
        1,
    );
    assert!(local.is_empty(), "a local gesture stays on this device");
}

/// Mac's only monitor is the top 100 rows of its 900-row box.
fn mac_top_strip() -> Vec<Rect> {
    vec![r(0, 0, 1440, 100)]
}

#[test]
fn crossing_back_announces_the_snapped_point_it_injects() {
    let mut e = engine(MAC);
    e.set_monitors(mac_top_strip());
    e.handle(mv(5, 0, 1439, 50), 0);
    assert_eq!(e.mode(), Mode::Driving(PC));
    e.handle(mv(0, 400, 0, 0), 1);
    let out = e.handle(mv(-5, 0, 0, 0), 2);
    let (_, who, x, y) = focus_of(&out).expect("focus broadcast");
    assert_eq!(who, MAC);
    assert!(out.contains(&Action::InjectMove { x, y }), "{out:?}");
    assert!(y <= 99, "snapped onto the monitor, not {y}");
}

#[test]
fn a_screen_whose_device_is_unknown_is_not_driven() {
    let d = Desk {
        version: 1,
        author: MAC,
        screens: vec![ds(MAC, 0, 100, 100), ds(PC, 100, 100, 100)],
    };
    let mut e = Engine::new(MAC, &d, BTreeMap::from([(MAC, Os::MacOs)])).unwrap();
    let out = e.handle(mv(5, 0, 99, 50), 0);
    assert_eq!((e.mode(), out), (Mode::Local, vec![]));
    // Still local afterwards: the cursor was not left parked on the unknown screen.
    assert!(e.handle(mv(1, 0, 50, 50), 1).is_empty());
}

#[test]
fn resizing_own_screen_resets_default_monitors_but_not_unrelated_edits() {
    let mut e = engine(MAC);
    let os = BTreeMap::from([(MAC, Os::MacOs), (PC, Os::Windows)]);
    // Mac grows to 2000x1000: a stale 1440x900 monitor would make the new area a hole.
    let big = Desk {
        version: 2,
        author: MAC,
        screens: vec![ds(MAC, 0, 2000, 1000), ds(PC, 2000, 1920, 1080)],
    };
    e.set_desk(&big, os.clone()).unwrap();
    let out = e.handle(focus(PC, MAC, 1900, 950, 0), 0);
    assert_eq!(out, vec![Action::InjectMove { x: 1900, y: 950 }]);
    // Custom monitors survive a same-size edit but not a resize.
    e.handle(focus(PC, PC, 0, 0, 1), 1);
    let mut e = engine(MAC);
    e.set_monitors(mac_top_strip());
    let same = Desk {
        version: 2,
        author: MAC,
        screens: vec![ds(MAC, 0, 1440, 900), ds(PC, 1440, 1000, 1000)],
    };
    e.set_desk(&same, os.clone()).unwrap();
    assert_eq!(e.monitors, mac_top_strip());
    e.set_desk(&big, os).unwrap();
    assert_eq!(e.monitors, vec![r(0, 0, 2000, 1000)]);
}

#[test]
fn duplicate_peer_desk_is_refused_with_a_reason() {
    let s = |name: &str, x| DeskScreen {
        peer: MAC,
        name: name.into(),
        x,
        y: 0,
        w: 10,
        h: 10,
    };
    let d = Desk {
        version: 1,
        author: MAC,
        screens: vec![s("a", 0), s("b", 10)],
    };
    assert_eq!(
        Engine::try_new(MAC, &d, BTreeMap::new()).err(),
        Some(LayoutError::DuplicatePeer("b".into()))
    );
    assert!(Engine::new(MAC, &d, BTreeMap::new()).is_none());
    let mut e = engine(MAC);
    assert_eq!(
        e.set_desk(&d, BTreeMap::new()).err(),
        Some(LayoutError::DuplicatePeer("b".into()))
    );
}

#[test]
fn handle_into_appends_to_a_reused_buffer_like_handle() {
    let ev = mv(5, 0, 1439, 450);
    let expected = engine(MAC).handle(ev.clone(), 0);
    let mut e = engine(MAC);
    let mut buf = vec![Action::ReleaseAll];
    e.handle_into(ev, 0, &mut buf);
    assert_eq!(buf[0], Action::ReleaseAll);
    assert_eq!(&buf[1..], &expected[..]);
}

#[test]
fn takeover_distance_does_not_overflow_on_extreme_deltas() {
    let mut e = driven(0);
    let out = e.handle(mv(i32::MIN, i32::MIN, 5, 5), 10_000);
    assert_eq!(e.mode(), Mode::Local);
    assert!(out.contains(&Action::ReleaseAll));
}
