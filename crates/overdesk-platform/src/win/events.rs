// SPDX-License-Identifier: AGPL-3.0-only
//! Pure Win32 input semantics.

use overdesk_proto::ScrollUnit;

/// One wheel notch (`WHEEL_DELTA`, 120) per this many pixels of Mac trackpad or Magic
/// Mouse scrolling. Windows apps scroll about 3 lines (~60 px) per notch.
pub const PX_PER_NOTCH: i32 = 60;

/// Wheel units for `MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL`. Small deltas scroll
/// smoothly in modern apps, and the Mac's momentum events supply the inertia.
pub fn wheel_units(delta: i32, unit: ScrollUnit) -> i32 {
    match unit {
        ScrollUnit::Pixel => delta.saturating_mul(120) / PX_PER_NOTCH,
        ScrollUnit::Notch120 => delta,
    }
}

/// `MOUSEEVENTF_ABSOLUTE` coordinates: 0..=65535 across the primary screen.
pub fn absolute(x: i32, y: i32, width: u32, height: u32) -> (i32, i32) {
    let scale = |v: i32, size: u32| {
        let max = (size.max(2) - 1) as i64;
        (v.clamp(0, max as i32) as i64 * 65535 / max) as i32
    };
    (scale(x, width), scale(y, height))
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
        assert_eq!(absolute(960, 540, 1920, 1080), (32784, 32797)); // 960*65535/1919, 540*65535/1079, truncated
        assert_eq!(absolute(-50, 5000, 1920, 1080), (0, 65535));
    }
}
