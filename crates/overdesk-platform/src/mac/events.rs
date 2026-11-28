// SPDX-License-Identifier: AGPL-3.0-only
//! Pure Quartz event semantics: what makes injected input indistinguishable from real
//! Apple hardware. Constants come from CGEventTypes.h and IOLLEvent.h.

use overdesk_proto::{Button, PAGE_APPLE_TOP_CASE, Phase};

use super::keymap::vk_to_code;
use crate::input::CAPS_CODE;

// Generic CGEventFlags.
/// `kCGEventFlagMaskShift`: set while either Shift key is down.
const FLAG_SHIFT: u64 = 0x0002_0000;
/// `kCGEventFlagMaskControl`: set while either Control key is down.
const FLAG_CONTROL: u64 = 0x0004_0000;
/// `kCGEventFlagMaskAlternate`: set while either Option key is down.
const FLAG_OPTION: u64 = 0x0008_0000;
/// `kCGEventFlagMaskCommand`: set while either Command key is down.
const FLAG_COMMAND: u64 = 0x0010_0000;
/// `kCGEventFlagMaskSecondaryFn`: set while the Fn/Globe key is down.
const FLAG_FN: u64 = 0x0080_0000;
// Device-dependent bits (NX_DEVICE*KEYMASK): tell left from right.
/// `NX_DEVICELCTLKEYMASK`: the left Control key is down.
const DEV_LCTL: u64 = 0x0001;
/// `NX_DEVICELSHIFTKEYMASK`: the left Shift key is down.
const DEV_LSHIFT: u64 = 0x0002;
/// `NX_DEVICERSHIFTKEYMASK`: the right Shift key is down.
const DEV_RSHIFT: u64 = 0x0004;
/// `NX_DEVICELCMDKEYMASK`: the left Command key is down.
const DEV_LCMD: u64 = 0x0008;
/// `NX_DEVICERCMDKEYMASK`: the right Command key is down.
const DEV_RCMD: u64 = 0x0010;
/// `NX_DEVICELALTKEYMASK`: the left Option key is down.
const DEV_LALT: u64 = 0x0020;
/// `NX_DEVICERALTKEYMASK`: the right Option key is down.
const DEV_RALT: u64 = 0x0040;
/// `NX_DEVICERCTLKEYMASK`: the right Control key is down.
const DEV_RCTL: u64 = 0x2000;

/// Overdesk code of the Fn/Globe key: usage 0x03 on Apple's vendor "top case" page.
const CODE_FN: u32 = PAGE_APPLE_TOP_CASE | 0x03;

/// `(overdesk code, bits that mean "this exact key is down", generic flag)`.
const MODIFIERS: &[(u32, u64, u64)] = &[
    (0xE0, DEV_LCTL, FLAG_CONTROL),
    (0xE1, DEV_LSHIFT, FLAG_SHIFT),
    (0xE2, DEV_LALT, FLAG_OPTION),
    (0xE3, DEV_LCMD, FLAG_COMMAND),
    (0xE4, DEV_RCTL, FLAG_CONTROL),
    (0xE5, DEV_RSHIFT, FLAG_SHIFT),
    (0xE6, DEV_RALT, FLAG_OPTION),
    (0xE7, DEV_RCMD, FLAG_COMMAND),
    // Fn has no side bit; its generic flag is the key state.
    (CODE_FN, FLAG_FN, FLAG_FN),
];

/// A modifier transition decoded from a `kCGEventFlagsChanged` event.
#[derive(Debug, PartialEq, Eq)]
pub enum ModChange {
    /// The modifier with this Overdesk code went down.
    Press(u32),
    /// The modifier with this Overdesk code came up.
    Release(u32),
    /// Caps Lock reports lock state, not key state; forward as press + release.
    Toggle(u32),
}

/// Decodes which modifier changed. macOS never sends key down/up for modifiers,
/// only `flagsChanged` with the key's `kVK` and the new flag word.
pub fn modifier_change(vk: u16, flags: u64) -> Option<ModChange> {
    let code = vk_to_code(vk)?;
    // Caps Lock's flag bit is the lock state, so it cannot say whether the key went
    // down or up; every flagsChanged for it is treated as one full tap.
    if code == CAPS_CODE {
        return Some(ModChange::Toggle(code));
    }
    // Only the device-dependent (side) bit is checked: the generic flag stays set
    // while the other side's key is still held.
    let &(_, down_bit, _) = MODIFIERS.iter().find(|m| m.0 == code)?;
    Some(if flags & down_bit != 0 {
        ModChange::Press(code)
    } else {
        ModChange::Release(code)
    })
}

/// Flag word an injected event must carry while `held` codes are down;
/// without it, Cmd+C typed from another computer arrives as a bare "c".
pub fn flags_for(held: &[u32]) -> u64 {
    MODIFIERS
        .iter()
        .filter(|m| held.contains(&m.0))
        .fold(0, |f, m| f | m.1 | m.2)
}

/// `kCGScrollWheelEventScrollPhase` + `kCGScrollWheelEventMomentumPhase` -> `Phase`.
pub fn phase_from_cg(scroll: i64, momentum: i64) -> Phase {
    match (scroll, momentum) {
        (_, 1) => Phase::MomentumBegan,
        (_, 2) => Phase::Momentum,
        (_, 3) => Phase::MomentumEnded,
        (1, _) => Phase::Began,
        (2, _) => Phase::Changed,
        (4, _) => Phase::Ended,
        (8, _) => Phase::Cancelled,
        (128, _) => Phase::MayBegin,
        _ => Phase::None,
    }
}

/// Inverse of [`phase_from_cg`].
pub fn phase_to_cg(p: Phase) -> (i64, i64) {
    match p {
        Phase::None => (0, 0),
        Phase::Began => (1, 0),
        Phase::Changed => (2, 0),
        Phase::Ended => (4, 0),
        Phase::Cancelled => (8, 0),
        Phase::MayBegin => (128, 0),
        Phase::MomentumBegan => (0, 1),
        Phase::Momentum => (0, 2),
        Phase::MomentumEnded => (0, 3),
    }
}

/// Event type for an injected pointer move. macOS apps only see drags
/// (text selection, window moves) when the move is typed as a drag.
#[derive(Debug, PartialEq, Eq)]
pub enum MoveKind {
    /// `kCGEventMouseMoved`: no button is held.
    Moved,
    /// `kCGEventLeftMouseDragged`: the left button is held.
    LeftDragged,
    /// `kCGEventRightMouseDragged`: the right button is held, but not the left.
    RightDragged,
    /// `kCGEventOtherMouseDragged`: only middle, back or forward buttons are held.
    OtherDragged,
}

/// Picks the Quartz move type for an injected pointer move from the buttons currently
/// held by the injector. The left button wins over the right, and the right over any
/// other, matching how macOS types real drags with several buttons down.
pub fn move_kind(held: &[Button]) -> MoveKind {
    if held.contains(&Button::Left) {
        MoveKind::LeftDragged
    } else if held.contains(&Button::Right) {
        MoveKind::RightDragged
    } else if held.is_empty() {
        MoveKind::Moved
    } else {
        MoveKind::OtherDragged
    }
}

/// Max distance in points between clicks of one multi-click, like real hardware.
const CLICK_SLOP: i32 = 4;

/// Computes `kCGMouseEventClickState`. Injected clicks without it never
/// double-click: Finder will not open files and text will not select words.
#[derive(Debug, Default)]
pub struct ClickCounter {
    /// The previous press as `(button, time in ms, x, y, click count)`, or `None`
    /// before the first press.
    last: Option<(Button, u64, i32, i32, i64)>,
}

impl ClickCounter {
    /// Records a press at `t_ms`; returns its click count (1 = single, 2 = double, ...).
    ///
    /// A press continues the previous multi-click when it is the same button, comes
    /// within `interval_ms` of the previous press and lands within `CLICK_SLOP` points
    /// of it on both axes; otherwise the count restarts at 1. Positions are in global
    /// display points.
    pub fn press(&mut self, button: Button, t_ms: u64, x: i32, y: i32, interval_ms: u64) -> i64 {
        let count = match self.last {
            Some((b, t, lx, ly, n))
                if b == button
                    && t_ms.saturating_sub(t) <= interval_ms
                    && (x - lx).abs() <= CLICK_SLOP
                    && (y - ly).abs() <= CLICK_SLOP =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last = Some((button, t_ms, x, y, count));
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Held, LocalDowns};

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

    #[test]
    fn release_of_locally_pressed_key_passes_while_swallowing() {
        let mut l = LocalDowns::default();
        assert!(l.deliver_locally(Held::Key(0xE3), true, false));
        assert!(
            l.deliver_locally(Held::Key(0xE3), false, true),
            "Cmd would stick on the Mac"
        );
    }

    #[test]
    fn input_while_swallowing_stays_off_this_mac() {
        let mut l = LocalDowns::default();
        assert!(!l.deliver_locally(Held::Button(Button::Left), true, true));
        assert!(!l.deliver_locally(Held::Button(Button::Left), false, true));
    }

    #[test]
    fn local_input_flows_when_not_swallowing() {
        let mut l = LocalDowns::default();
        // Release of a key pressed before capture started.
        assert!(l.deliver_locally(Held::Key(0x04), false, false));
        assert!(l.deliver_locally(Held::Key(0x04), true, false));
        assert!(l.deliver_locally(Held::Key(0x04), false, false));
        // Already released: a later release while swallowing is not local.
        assert!(!l.deliver_locally(Held::Key(0x04), false, true));
    }
}
