// SPDX-License-Identifier: AGPL-3.0-only
//! Pure Win32 input semantics.

use overdesk_proto::ScrollUnit;

/// One wheel notch (`WHEEL_DELTA`, 120) per this many pixels of Mac trackpad or Magic
/// Mouse scrolling. Windows apps scroll about 3 lines (~60 px) per notch.
pub const PX_PER_NOTCH: i32 = 60;

/// Wheel units for `MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL`. Small deltas scroll
/// smoothly in modern apps, and the Mac's momentum events supply the inertia.
pub fn wheel_units(_delta: i32, _unit: ScrollUnit) -> i32 {
    todo!()
}

/// `MOUSEEVENTF_ABSOLUTE` coordinates: 0..=65535 across the primary screen.
pub fn absolute(_x: i32, _y: i32, _width: u32, _height: u32) -> (i32, i32) {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_pixels_become_smooth_wheel_units() {
        assert_eq!(wheel_units(-30, ScrollUnit::Pixel), -60, "half a notch down");
        assert_eq!(wheel_units(PX_PER_NOTCH, ScrollUnit::Pixel), 120, "one notch");
        assert_eq!(wheel_units(-120, ScrollUnit::Notch120), -120, "notches pass through");
    }

    #[test]
    fn absolute_spans_the_screen_and_clamps() {
        assert_eq!(absolute(0, 0, 1920, 1080), (0, 0));
        assert_eq!(absolute(1919, 1079, 1920, 1080), (65535, 65535));
        assert_eq!(absolute(960, 540, 1920, 1080), (32784, 32798));
        assert_eq!(absolute(-50, 5000, 1920, 1080), (0, 65535));
    }
}
