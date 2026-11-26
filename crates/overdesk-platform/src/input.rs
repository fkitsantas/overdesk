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
        dx: i32,
        dy: i32,
        x: i32,
        y: i32,
    },
    Button {
        button: Button,
        pressed: bool,
    },
    Key {
        code: u32,
        pressed: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
        unit: ScrollUnit,
        phase: Phase,
    },
    /// The user pressed the escape chord: local input is back, whatever the engine thinks.
    Escape,
}

/// Control+Option/Alt+Command/Win+Escape, left or right modifiers. Detected in the
/// capture layer itself so a hung engine or dead peer can never lock the user out.
#[derive(Debug, Default)]
pub struct EscapeChord {
    held: BTreeSet<u32>,
}

impl EscapeChord {
    /// Feeds one key edge; returns `true` on the press that completes the chord.
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        const ESC: u32 = 0x29;
        if !pressed {
            self.held.remove(&code);
            return false;
        }
        let fresh = self.held.insert(code);
        let either = |l: u32, r: u32| self.held.contains(&l) || self.held.contains(&r);
        fresh && code == ESC && either(0xE0, 0xE4) && either(0xE2, 0xE6) && either(0xE3, 0xE7)
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
