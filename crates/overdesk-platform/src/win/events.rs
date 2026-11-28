// SPDX-License-Identifier: AGPL-3.0-only
//! Pure Win32 input semantics.

/// The scan code Windows gives the Ctrl it injects ahead of AltGr (`0x21D`, a left Ctrl with a
/// bit no physical keyboard sets). It is an artefact of AltGr, never a key the user pressed.
pub const FAKE_LCTRL_SCAN: u32 = 0x21D;

/// Whether a key event the adapter does not forward (an unmapped key, or AltGr's fake Ctrl)
/// reaches local apps. Presses are swallowed while swallowing; releases always pass, so a key
/// pressed before swallowing began cannot stay stuck down.
pub fn passes_unforwarded_key(down: bool, swallowing: bool) -> bool {
    !down || !swallowing
}

/// Follows the Caps Lock toggle from the keys that actually reached the OS, because the OS
/// cannot be asked from a thread without a message queue: `GetKeyState` there is a stale
/// snapshot. Seed it from a live read on the capture thread, then feed every Caps Lock edge.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CapsTracker {
    /// Whether Caps Lock is believed on.
    on: bool,
    /// Whether the key is currently down (auto-repeat must not toggle again).
    down: bool,
}

impl CapsTracker {
    /// A tracker that starts with Caps Lock `on`.
    pub fn new(on: bool) -> Self {
        Self { on, down: false }
    }

    /// Feeds one Caps Lock edge. `delivered` is whether it reached the OS (a swallowed press
    /// toggles nothing). Returns the belief after the edge.
    pub fn feed(&mut self, down: bool, delivered: bool) -> bool {
        if down && !self.down && delivered {
            self.on = !self.on;
        }
        self.down = down;
        self.on
    }
}

/// `MOUSEEVENTF_ABSOLUTE` coordinates: 0..=65535 across the virtual screen
/// (pair with `MOUSEEVENTF_VIRTUALDESK`); `x`/`y` are relative to its origin.
pub fn absolute(x: i32, y: i32, width: u32, height: u32) -> (i32, i32) {
    let scale = |v: i32, size: u32| {
        let max = (size.max(2) - 1) as i64;
        (v.clamp(0, max as i32) as i64 * 65535 / max) as i32
    };
    (scale(x, width), scale(y, height))
}

/// Windows pointer-speed slider (1..=20, default 10) as a multiplier: 1/32, 1/16, 1/8,
/// then 2/8..8/8 for 4..10, then 1.25 up to 3.5 for 11..20 (Microsoft's documented table).
pub fn speed_factor(speed: i32) -> f64 {
    match speed.clamp(1, 20) {
        1 => 1.0 / 32.0,
        2 => 1.0 / 16.0,
        3 => 1.0 / 8.0,
        s @ 4..=10 => f64::from(s - 2) / 8.0,
        s => 1.0 + 0.25 * f64::from(s - 10),
    }
}

/// One `SmoothMouse*Curve` registry entry: the low DWORD of each 8-byte point, 16.16
/// fixed point.
pub fn fixed_16_16(raw: u32) -> f64 {
    f64::from(raw) / 65536.0
}

/// The user's pointer settings, applied to Raw Input deltas that bypass them.
#[derive(Debug, Clone, PartialEq)]
pub struct Ballistics {
    /// Slider multiplier from [`speed_factor`] times the DPI factor (96 / actual DPI).
    scale: f64,
    /// `(speed, output speed)` points of "Enhance pointer precision", in counts per ms;
    /// `None` when it is off or the curve could not be read.
    curve: Option<[(f64, f64); 5]>,
    /// Sub-pixel remainder carried to the next event, per axis.
    rest: (f64, f64),
}

impl Ballistics {
    /// Builds the model from `SPI_GETMOUSESPEED`, `SPI_GETMOUSE`'s acceleration flag, the
    /// DPI factor and (only used when `enhance`) the X and Y curve points as 16.16 DWORDs.
    pub fn new(speed: i32, enhance: bool, dpi_factor: f64, curve: Option<([u32; 5], [u32; 5])>) -> Self {
        let curve = curve
            .filter(|_| enhance)
            .map(|(xs, ys)| std::array::from_fn(|i| (fixed_16_16(xs[i]), fixed_16_16(ys[i]))));
        Self {
            scale: speed_factor(speed) * dpi_factor,
            curve,
            rest: (0.0, 0.0),
        }
    }

    /// Pointer gain at `v` counts per ms: piecewise-linear interpolation of the curve
    /// (extending the last segment past its end) divided by the speed, or 1 without a curve.
    fn gain(&self, v: f64) -> f64 {
        let Some(c) = &self.curve else { return 1.0 };
        if v <= 0.0 {
            return 1.0;
        }
        let i = (1..4).find(|&i| v < c[i].0).unwrap_or(4);
        let ((x0, y0), (x1, y1)) = (c[i - 1], c[i]);
        if x1 <= x0 {
            return 1.0;
        }
        (y0 + (v - x0) * (y1 - y0) / (x1 - x0)) / v
    }

    /// Converts one raw delta, `dt_ms` after the previous one, to cursor pixels.
    pub fn apply(&mut self, dx: i32, dy: i32, dt_ms: f64) -> (i32, i32) {
        let v = f64::from(dx).hypot(f64::from(dy)) / dt_ms.max(1.0);
        let k = self.gain(v) * self.scale;
        let axis = |d: i32, rest: f64| {
            let t = f64::from(d) * k + rest;
            (t.trunc(), t.fract())
        };
        let (ox, rx) = axis(dx, self.rest.0);
        let (oy, ry) = axis(dy, self.rest.1);
        self.rest = (rx, ry);
        (ox as i32, oy as i32)
    }
}

/// Whether a clipboard privacy marker present on the clipboard means "keep private".
/// `ExcludeClipboardContentFromMonitorProcessing` excludes by presence alone; the two
/// `Can...` formats carry a DWORD where 0 forbids history or cloud use and 1 allows it.
/// An unreadable value (`None`) counts as private.
pub fn marker_is_private(value_matters: bool, value: Option<u32>) -> bool {
    !value_matters || value.is_none_or(|v| v == 0)
}

/// How long Raw Input may run ahead of a low-level hook's last call before the hook counts as
/// removed. Normally the hook runs just before (or just after) the same device's raw report.
pub const HOOK_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether Windows has silently removed a low-level hook (it does so after a callback times
/// out): Raw Input kept arriving, from a physical device, more than [`HOOK_GRACE`] after the
/// hook last ran. `last_raw` is `None` until a report arrives. Leave `last_hook` at the
/// install time after a reinstall so a quiet device is not reinstalled again and again.
pub fn hook_is_dead(last_hook: std::time::Instant, last_raw: Option<std::time::Instant>) -> bool {
    last_raw.is_some_and(|raw| raw.saturating_duration_since(last_hook) > HOOK_GRACE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_dead_only_when_raw_input_outruns_it() {
        use std::time::{Duration, Instant};
        let t = Instant::now();
        let ms = Duration::from_millis;
        assert!(!hook_is_dead(t, None), "no raw input yet");
        assert!(!hook_is_dead(t, Some(t)), "same instant");
        assert!(!hook_is_dead(t + ms(100), Some(t)), "raw older than the hook");
        assert!(!hook_is_dead(t, Some(t + ms(500))), "exactly the grace");
        assert!(hook_is_dead(t, Some(t + ms(501))), "raw well past the hook");
    }

    #[test]
    fn unforwarded_releases_always_pass() {
        assert!(passes_unforwarded_key(false, true));
        assert!(passes_unforwarded_key(true, false));
        assert!(!passes_unforwarded_key(true, true));
    }

    #[test]
    fn caps_toggles_once_per_delivered_press() {
        let mut c = CapsTracker::new(false);
        assert!(c.feed(true, true));
        assert!(c.feed(true, true), "auto-repeat does not toggle again");
        assert!(c.feed(false, true));
        assert!(!c.feed(true, true));
        c.feed(false, true);
        assert!(!c.feed(true, false), "a swallowed press never reached the OS");
    }

    #[test]
    fn speed_table_matches_documentation() {
        let want = [
            (1, 1.0 / 32.0),
            (3, 0.125),
            (4, 0.25),
            (6, 0.5),
            (10, 1.0),
            (11, 1.25),
            (20, 3.5),
        ];
        for (s, f) in want {
            assert_eq!(speed_factor(s), f, "speed {s}");
        }
        assert_eq!(speed_factor(99), 3.5);
    }

    #[test]
    fn precision_off_scales_linearly_and_keeps_remainder() {
        let mut b = Ballistics::new(5, false, 1.0, None); // 3/8
        assert_eq!(b.apply(8, -8, 1.0), (3, -3));
        assert_eq!(b.apply(1, 1, 1.0), (0, 0), "0.375 carried, not lost");
        let total: i32 = (0..8).map(|_| b.apply(1, 0, 1.0).0).sum();
        assert_eq!(total, 3, "8 more counts at 3/8 add 3 pixels");
    }

    #[test]
    fn precision_on_interpolates_the_curve() {
        // Curve y = 2x on every segment: constant gain 2, including past the last point.
        let xs = [0, 0x10000, 0x20000, 0x30000, 0x40000];
        let ys = [0, 0x20000, 0x40000, 0x60000, 0x80000];
        let mut b = Ballistics::new(10, true, 1.0, Some((xs, ys)));
        assert_eq!(b.apply(3, 0, 1.0), (6, 0));
        assert_eq!(b.apply(20, 0, 1.0), (40, 0), "extrapolated beyond the last point");
        assert!((b.gain(0.5) - 2.0).abs() < 1e-9);
        // Off, or unreadable curve: plain speed scaling.
        assert_eq!(Ballistics::new(20, false, 1.0, Some((xs, ys))).apply(2, 0, 1.0), (7, 0));
        assert_eq!(
            Ballistics::new(10, true, 2.0, None).apply(2, 0, 1.0),
            (4, 0),
            "DPI factor"
        );
    }

    #[test]
    fn clipboard_markers() {
        assert!(marker_is_private(false, Some(1)), "exclude marker: presence is enough");
        assert!(marker_is_private(true, Some(0)));
        assert!(!marker_is_private(true, Some(1)));
        assert!(marker_is_private(true, None), "fail closed");
    }

    #[test]
    fn absolute_spans_the_screen_and_clamps() {
        assert_eq!(absolute(0, 0, 1920, 1080), (0, 0));
        assert_eq!(absolute(1919, 1079, 1920, 1080), (65535, 65535));
        assert_eq!(absolute(960, 540, 1920, 1080), (32784, 32797)); // 960*65535/1919, 540*65535/1079, truncated
        assert_eq!(absolute(-50, 5000, 1920, 1080), (0, 65535));
    }
}
