// SPDX-License-Identifier: AGPL-3.0-only
//! Pure Quartz event semantics: what makes injected input indistinguishable from real
//! Apple hardware. Constants come from CGEventTypes.h and IOLLEvent.h.

use overdesk_proto::{Button, Phase};

// Generic CGEventFlags.
pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_OPTION: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;
pub const FLAG_FN: u64 = 0x0080_0000;
// Device-dependent bits (NX_DEVICE*KEYMASK): tell left from right.
pub const DEV_LCTL: u64 = 0x0001;
pub const DEV_LSHIFT: u64 = 0x0002;
pub const DEV_RSHIFT: u64 = 0x0004;
pub const DEV_LCMD: u64 = 0x0008;
pub const DEV_RCMD: u64 = 0x0010;
pub const DEV_LALT: u64 = 0x0020;
pub const DEV_RALT: u64 = 0x0040;
pub const DEV_RCTL: u64 = 0x2000;

/// A modifier transition decoded from a `kCGEventFlagsChanged` event.
#[derive(Debug, PartialEq, Eq)]
pub enum ModChange {
    Press(u32),
    Release(u32),
    /// Caps Lock reports lock state, not key state; forward as press + release.
    Toggle(u32),
}

/// Decodes which modifier changed. macOS never sends key down/up for modifiers,
/// only `flagsChanged` with the key's `kVK` and the new flag word.
pub fn modifier_change(_vk: u16, _flags: u64) -> Option<ModChange> {
    todo!()
}

/// Flag word an injected event must carry while `held` codes are down;
/// without it, Cmd+C typed from another computer arrives as a bare "c".
pub fn flags_for(_held: &[u32]) -> u64 {
    todo!()
}

/// `kCGScrollWheelEventScrollPhase` + `kCGScrollWheelEventMomentumPhase` -> `Phase`.
pub fn phase_from_cg(_scroll: i64, _momentum: i64) -> Phase {
    todo!()
}

/// Inverse of [`phase_from_cg`].
pub fn phase_to_cg(_p: Phase) -> (i64, i64) {
    todo!()
}

/// Event type for an injected pointer move. macOS apps only see drags
/// (text selection, window moves) when the move is typed as a drag.
#[derive(Debug, PartialEq, Eq)]
pub enum MoveKind {
    Moved,
    LeftDragged,
    RightDragged,
    OtherDragged,
}

pub fn move_kind(_held: &[Button]) -> MoveKind {
    todo!()
}

/// Max distance in points between clicks of one multi-click, like real hardware.
pub const CLICK_SLOP: i32 = 4;

/// Computes `kCGMouseEventClickState`. Injected clicks without it never
/// double-click: Finder will not open files and text will not select words.
#[derive(Debug, Default)]
pub struct ClickCounter {
    last: Option<(Button, u64, i32, i32, i64)>,
}

impl ClickCounter {
    /// Records a press at `t_ms`; returns its click count (1 = single, 2 = double, ...).
    pub fn press(&mut self, _button: Button, _t_ms: u64, _x: i32, _y: i32, _interval_ms: u64) -> i64 {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VK_LCMD: u16 = 0x37;
    const VK_RCMD: u16 = 0x36;
    const VK_LSHIFT: u16 = 0x38;
    const VK_CAPS: u16 = 0x39;
    const VK_FN: u16 = 0x3F;

    #[test]
    fn left_command_press_and_release() {
        assert_eq!(
            modifier_change(VK_LCMD, FLAG_COMMAND | DEV_LCMD),
            Some(ModChange::Press(0xE3))
        );
        assert_eq!(modifier_change(VK_LCMD, 0), Some(ModChange::Release(0xE3)));
    }

    #[test]
    fn releasing_left_command_while_right_held_is_a_release() {
        // Generic Command flag stays set because right Command is still down.
        assert_eq!(
            modifier_change(VK_LCMD, FLAG_COMMAND | DEV_RCMD),
            Some(ModChange::Release(0xE3))
        );
        assert_eq!(
            modifier_change(VK_RCMD, FLAG_COMMAND | DEV_RCMD),
            Some(ModChange::Press(0xE7))
        );
    }

    #[test]
    fn fn_globe_and_caps_lock() {
        let fn_code = overdesk_proto::PAGE_APPLE_TOP_CASE | 0x03;
        assert_eq!(modifier_change(VK_FN, FLAG_FN), Some(ModChange::Press(fn_code)));
        assert_eq!(modifier_change(VK_FN, 0), Some(ModChange::Release(fn_code)));
        assert_eq!(modifier_change(VK_CAPS, 0), Some(ModChange::Toggle(0x39)));
    }

    #[test]
    fn non_modifier_key_is_not_a_mod_change() {
        assert_eq!(modifier_change(0x00, FLAG_SHIFT | DEV_LSHIFT), None);
        let _ = VK_LSHIFT;
    }

    #[test]
    fn flags_carry_generic_and_side_bits() {
        assert_eq!(flags_for(&[]), 0);
        assert_eq!(flags_for(&[0xE3, 0x06]), FLAG_COMMAND | DEV_LCMD);
        assert_eq!(flags_for(&[0xE1, 0xE5]), FLAG_SHIFT | DEV_LSHIFT | DEV_RSHIFT);
        assert_eq!(
            flags_for(&[0xE4, 0xE6]),
            FLAG_CONTROL | DEV_RCTL | FLAG_OPTION | DEV_RALT
        );
        assert_eq!(flags_for(&[overdesk_proto::PAGE_APPLE_TOP_CASE | 0x03]), FLAG_FN);
    }

    #[test]
    fn scroll_phases_round_trip() {
        use Phase::*;
        for p in [
            None,
            Began,
            Changed,
            Ended,
            Cancelled,
            MayBegin,
            MomentumBegan,
            Momentum,
            MomentumEnded,
        ] {
            let (s, m) = phase_to_cg(p);
            assert_eq!(phase_from_cg(s, m), p, "{p:?}");
        }
        assert_eq!(phase_to_cg(Began), (1, 0));
        assert_eq!(phase_to_cg(MayBegin), (128, 0));
        assert_eq!(phase_to_cg(Momentum), (0, 2));
        assert_eq!(phase_from_cg(77, 0), None);
    }

    #[test]
    fn moves_with_buttons_held_are_drags() {
        assert_eq!(move_kind(&[]), MoveKind::Moved);
        assert_eq!(move_kind(&[Button::Right, Button::Left]), MoveKind::LeftDragged);
        assert_eq!(move_kind(&[Button::Right]), MoveKind::RightDragged);
        assert_eq!(move_kind(&[Button::Middle]), MoveKind::OtherDragged);
    }

    #[test]
    fn double_and_triple_click() {
        let mut c = ClickCounter::default();
        assert_eq!(c.press(Button::Left, 1000, 10, 10, 500), 1);
        assert_eq!(c.press(Button::Left, 1300, 12, 9, 500), 2);
        assert_eq!(c.press(Button::Left, 1700, 12, 9, 500), 3);
    }

    #[test]
    fn slow_far_or_other_button_resets_count() {
        let mut c = ClickCounter::default();
        c.press(Button::Left, 0, 0, 0, 500);
        assert_eq!(c.press(Button::Left, 501, 0, 0, 500), 1, "too slow");
        assert_eq!(c.press(Button::Left, 600, CLICK_SLOP + 1, 0, 500), 1, "too far");
        assert_eq!(c.press(Button::Right, 700, CLICK_SLOP + 1, 0, 500), 1, "other button");
    }
}
