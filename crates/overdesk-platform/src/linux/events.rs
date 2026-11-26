// SPDX-License-Identifier: AGPL-3.0-only
//! Pure X11 input semantics.

use overdesk_proto::{Button, ScrollUnit};

/// One wheel click per this many pixels of Mac trackpad or Magic Mouse scrolling. X11 has
/// no smooth wheel in the core protocol, only clicks (about 3 lines each, ~60 px).
pub const PX_PER_NOTCH: i32 = 60;

/// A `Notch120` delta is 1/120 of a click.
const UNITS_PER_NOTCH: i32 = 120;

/// Turns scroll deltas into whole wheel clicks, keeping the fractional rest so slow
/// trackpad scrolling still moves the page. Units are 1/120 click; the remainder keeps
/// the sign of the motion.
#[derive(Debug, Default)]
pub struct Notches {
    /// Horizontal remainder, `|x| < 120`.
    x: i32,
    /// Vertical remainder, `|y| < 120`.
    y: i32,
}

impl Notches {
    /// Adds one scroll event and returns the whole `(horizontal, vertical)` clicks now due.
    pub fn feed(&mut self, dx: i32, dy: i32, unit: ScrollUnit) -> (i32, i32) {
        let scale = |d: i32| match unit {
            ScrollUnit::Pixel => d.saturating_mul(UNITS_PER_NOTCH) / PX_PER_NOTCH,
            ScrollUnit::Notch120 => d,
        };
        self.x = self.x.saturating_add(scale(dx));
        self.y = self.y.saturating_add(scale(dy));
        let (cx, cy) = (self.x / UNITS_PER_NOTCH, self.y / UNITS_PER_NOTCH);
        self.x %= UNITS_PER_NOTCH;
        self.y %= UNITS_PER_NOTCH;
        (cx, cy)
    }
}

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
    fn trackpad_pixels_accumulate_into_clicks() {
        let mut n = Notches::default();
        assert_eq!(n.feed(0, -20, ScrollUnit::Pixel), (0, 0));
        assert_eq!(n.feed(0, -20, ScrollUnit::Pixel), (0, 0));
        assert_eq!(n.feed(0, -20, ScrollUnit::Pixel), (0, -1), "60 px is one click down");
        assert_eq!(n.feed(90, 0, ScrollUnit::Pixel), (1, 0));
        assert_eq!(n.feed(30, 0, ScrollUnit::Pixel), (1, 0), "the half click carried over");
    }

    #[test]
    fn notches_pass_one_to_one() {
        let mut n = Notches::default();
        assert_eq!(n.feed(0, 240, ScrollUnit::Notch120), (0, 2));
        assert_eq!(n.feed(-120, 60, ScrollUnit::Notch120), (-1, 0));
        assert_eq!(n.feed(0, 60, ScrollUnit::Notch120), (0, 1));
    }

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
