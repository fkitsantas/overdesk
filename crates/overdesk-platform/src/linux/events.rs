// SPDX-License-Identifier: AGPL-3.0-only
//! Pure X11 input semantics.

use overdesk_proto::Button;

use crate::input::UNITS_PER_NOTCH;

/// X11 button number for `button` (1 left, 2 middle, 3 right; 4-7 are the wheel).
pub fn x_button(button: Button) -> u8 {
    match button {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
        Button::Back => 8,
        Button::Forward => 9,
    }
}

/// The Overdesk button for X11 button number `n`, or `None` for the wheel (4-7) and
/// anything else.
pub fn button_from_x(n: u32) -> Option<Button> {
    Some(match n {
        1 => Button::Left,
        2 => Button::Middle,
        3 => Button::Right,
        8 => Button::Back,
        9 => Button::Forward,
        _ => return None,
    })
}

/// A wheel button press (4 up, 5 down, 6 left, 7 right) as `(dx, dy)` in 1/120 clicks,
/// positive right and up like `Captured::Scroll`.
pub fn wheel_from_x(n: u32) -> Option<(i32, i32)> {
    Some(match n {
        4 => (0, UNITS_PER_NOTCH),
        5 => (0, -UNITS_PER_NOTCH),
        6 => (-UNITS_PER_NOTCH, 0),
        7 => (UNITS_PER_NOTCH, 0),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_round_trip_and_wheel_is_not_a_button() {
        for b in [
            Button::Left,
            Button::Middle,
            Button::Right,
            Button::Back,
            Button::Forward,
        ] {
            assert_eq!(button_from_x(u32::from(x_button(b))), Some(b));
        }
        assert_eq!(button_from_x(4), None);
        assert_eq!(wheel_from_x(5), Some((0, -120)));
        assert_eq!(wheel_from_x(6), Some((-120, 0)));
        assert_eq!(wheel_from_x(1), None);
    }
}
