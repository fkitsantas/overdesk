// SPDX-License-Identifier: AGPL-3.0-only
//! Platform-neutral captured input, shared by every OS adapter and the engine.

use std::collections::BTreeSet;

use overdesk_proto::{Button, Phase, ScrollUnit};

/// Physical input seen on this device, in Overdesk terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Captured {
    /// Relative motion plus where the system cursor is now (global points), so the
    /// engine never dead-reckons the local cursor.
    Moved {
        /// Horizontal motion since the previous event, in device units (pixels or
        /// points), positive to the right. Taken from the hardware delta, not from the
        /// difference between cursor positions, so it keeps flowing at screen edges.
        dx: i32,
        /// Vertical motion since the previous event, in device units, positive downwards.
        dy: i32,
        /// Current horizontal cursor position in global display coordinates.
        x: i32,
        /// Current vertical cursor position in global display coordinates (top-left origin).
        y: i32,
    },
    /// A mouse button was pressed or released.
    Button {
        /// Which physical button changed state.
        button: Button,
        /// `true` for a press, `false` for a release.
        pressed: bool,
    },
    /// A key was pressed, repeated or released.
    Key {
        /// The key as an Overdesk code: a USB HID keyboard-page usage, or a usage on
        /// another HID page (consumer, Apple top case) tagged with its page prefix.
        code: u32,
        /// `true` for a press (including auto-repeat), `false` for a release.
        pressed: bool,
    },
    /// A scroll wheel, trackpad or Magic Mouse scroll step.
    Scroll {
        /// Horizontal scroll amount in `unit`s, positive to the right.
        dx: i32,
        /// Vertical scroll amount in `unit`s, positive upwards (content moves down).
        dy: i32,
        /// Whether the deltas are continuous pixels or 1/120 wheel notches.
        unit: ScrollUnit,
        /// Gesture or momentum phase, so the receiver can reproduce inertial scrolling.
        phase: Phase,
    },
    /// The user pressed the escape chord: local input is back, whatever the engine thinks.
    Escape,
}

/// Control+Option/Alt+Command/Win+Escape, left or right modifiers. Detected in the
/// capture layer itself so a hung engine or dead peer can never lock the user out.
#[derive(Debug, Default)]
pub struct EscapeChord {
    /// Overdesk codes of every key currently held down, as seen by this detector.
    held: BTreeSet<u32>,
}

impl EscapeChord {
    /// Feeds one key edge; returns `true` on the press that completes the chord.
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        /// HID keyboard-page usage of the Escape key.
        const ESC: u32 = 0x29;
        if !pressed {
            self.held.remove(&code);
            return false;
        }
        // `insert` is false for a key already held, so auto-repeat of Escape never
        // fires the chord a second time.
        let fresh = self.held.insert(code);
        // HID usages: 0xE0/0xE4 Control, 0xE2/0xE6 Option/Alt, 0xE3/0xE7 Command/Win,
        // left and right respectively; either side satisfies each modifier.
        let either = |l: u32, r: u32| self.held.contains(&l) || self.held.contains(&r);
        fresh && code == ESC && either(0xE0, 0xE4) && either(0xE2, 0xE6) && either(0xE3, 0xE7)
    }
}

/// Something that can be held down locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Held {
    /// A keyboard key, identified by its Overdesk code.
    Key(u32),
    /// A mouse button.
    Button(Button),
}

/// Decides whether a captured press/release also reaches local apps. Releases of
/// anything pressed while local input was live always pass, so turning swallowing on
/// mid-press can never leave a key or drag stuck on this Mac.
#[derive(Debug, Default)]
pub struct LocalDowns(
    /// Keys and buttons whose press was delivered to local apps and whose release has
    /// not been seen yet.
    BTreeSet<Held>,
);

impl LocalDowns {
    /// Records one press (`down`) or release edge and returns whether the event should
    /// reach local apps. `swallowing` is `true` while focus is on another peer.
    ///
    /// Presses pass only while not swallowing, and are remembered when they do. A
    /// release passes if its press was delivered locally, or if nothing is being
    /// swallowed, so local apps always see balanced press/release pairs.
    pub fn deliver_locally(&mut self, held: Held, down: bool, swallowing: bool) -> bool {
        if down {
            if !swallowing {
                self.0.insert(held);
            }
            !swallowing
        } else {
            self.0.remove(&held) || !swallowing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L_CTRL: u32 = 0xE0;
    const L_ALT: u32 = 0xE2;
    const L_GUI: u32 = 0xE3;
    const R_GUI: u32 = 0xE7;
    const ESC: u32 = 0x29;

    #[test]
    fn chord_fires_once_on_escape_with_all_three_modifiers() {
        let mut c = EscapeChord::default();
        assert!(!c.key(L_CTRL, true));
        assert!(!c.key(L_ALT, true));
        assert!(!c.key(R_GUI, true));
        assert!(c.key(ESC, true));
        assert!(!c.key(ESC, true), "key repeat must not re-fire");
        assert!(!c.key(ESC, false));
        assert!(c.key(ESC, true), "a fresh press fires again");
    }

    #[test]
    fn partial_chords_do_nothing() {
        let mut c = EscapeChord::default();
        c.key(L_ALT, true);
        c.key(L_GUI, true);
        assert!(!c.key(ESC, true), "Cmd+Opt+Esc is Force Quit, not ours");
        c.key(L_CTRL, true);
        c.key(L_GUI, false);
        c.key(ESC, false);
        assert!(!c.key(ESC, true), "Command was released");
    }
}
