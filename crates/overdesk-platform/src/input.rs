// SPDX-License-Identifier: AGPL-3.0-only
//! Platform-neutral captured input, shared by every OS adapter and the engine.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

pub use overdesk_proto::Rect;
use overdesk_proto::{Button, Gesture, Phase, ScrollUnit};

/// One wheel notch (1/120 units: `WHEEL_DELTA`, an X11 wheel click) per this many pixels of
/// Mac trackpad or Magic Mouse scrolling. Apps scroll about 3 lines (~60 px) per notch.
pub const PX_PER_NOTCH: i32 = 60;

/// One wheel notch in the 1/120 units of [`ScrollUnit::Notch120`].
pub const UNITS_PER_NOTCH: i32 = 120;

/// A scroll delta in `unit`s as 1/120 wheel notches (what Windows and X11 wheels speak).
pub fn to_units(delta: i32, unit: ScrollUnit) -> i32 {
    match unit {
        ScrollUnit::Pixel => delta.saturating_mul(UNITS_PER_NOTCH) / PX_PER_NOTCH,
        ScrollUnit::Notch120 => delta,
    }
}

/// Turns scroll deltas into whole wheel notches, keeping the fractional rest so slow
/// trackpad scrolling and high-resolution wheels still move the page. A remainder pointing
/// the other way than the new delta is dropped (the direction changed).
#[derive(Debug, Default)]
pub struct Notches {
    /// Horizontal remainder in 1/120 notch, `|x| < 120`.
    x: i32,
    /// Vertical remainder in 1/120 notch, `|y| < 120`.
    y: i32,
}

impl Notches {
    /// Adds one scroll event and returns the whole `(horizontal, vertical)` notches now due.
    pub fn feed(&mut self, dx: i32, dy: i32, unit: ScrollUnit) -> (i32, i32) {
        fn axis(rem: &mut i32, d: i32) -> i32 {
            if d != 0 && (*rem < 0) != (d < 0) {
                *rem = 0;
            }
            let total = rem.saturating_add(d);
            *rem = total % UNITS_PER_NOTCH;
            total / UNITS_PER_NOTCH
        }
        (
            axis(&mut self.x, to_units(dx, unit)),
            axis(&mut self.y, to_units(dy, unit)),
        )
    }
}

/// Carries fractional mouse deltas between events so slow, precise trackpad and
/// Magic Mouse motion is not rounded away.
#[derive(Debug, Default)]
pub struct Subpixel {
    // Fixed point in 1/1024 px: float remainders drift (0.2 - 1.2 != -1.0).
    /// Horizontal remainder not yet emitted, in 1/1024 px; always `-1024 < rx < 1024`.
    rx: i64,
    /// Vertical remainder not yet emitted, in 1/1024 px; always `-1024 < ry < 1024`.
    ry: i64,
}

impl Subpixel {
    /// Adds one event's fractional delta (points) and returns the whole points that are
    /// now due, keeping the fractional rest for the next call.
    pub fn feed(&mut self, dx: f64, dy: f64) -> (i32, i32) {
        /// One whole point in the fixed-point scale.
        const ONE: i64 = 1024;
        self.rx += (dx * ONE as f64).round() as i64;
        self.ry += (dy * ONE as f64).round() as i64;
        // Integer division and `%` both truncate towards zero, so the remainder keeps
        // the sign of the motion and negative moves round the same way as positive ones.
        let (ix, iy) = (self.rx / ONE, self.ry / ONE);
        self.rx %= ONE;
        self.ry %= ONE;
        (ix as i32, iy as i32)
    }
}

/// The bounding box of `rects`, or `None` when there are none. This is a device's
/// "screen" on the Desk: capture reports positions relative to its origin and injection
/// adds the origin back, so OS origins may be negative. The monitors need not fill it;
/// the engine gets their rectangles from [`box_relative`] for the holes.
pub fn bounding_box(rects: impl IntoIterator<Item = Rect>) -> Option<Rect> {
    rects.into_iter().fold(None, |acc, rect| {
        let Some(acc) = acc else {
            return Some(rect);
        };
        let (nx, ny) = (acc.x.min(rect.x), acc.y.min(rect.y));
        let (nr, nb) = (acc.right().max(rect.right()), acc.bottom().max(rect.bottom()));
        Some(Rect {
            x: nx,
            y: ny,
            w: (nr - i64::from(nx)) as u32,
            h: (nb - i64::from(ny)) as u32,
        })
    })
}

/// `rects` relative to their own [`bounding_box`] origin, the form the engine takes.
pub fn box_relative(rects: &[Rect]) -> Vec<Rect> {
    let (ox, oy) = bounding_box(rects.iter().copied()).map_or((0, 0), |b| (b.x, b.y));
    rects
        .iter()
        .map(|r| Rect {
            x: to_local(r.x, ox),
            y: to_local(r.y, oy),
            ..*r
        })
        .collect()
}

/// Global OS coordinates to box-relative ones (`origin` is the box's top-left).
pub fn to_local(global: i32, origin: i32) -> i32 {
    global.saturating_sub(origin)
}

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
    /// A trackpad gesture (Mac only). The open daemon drops it; Pro forwards it.
    Gesture {
        /// The gesture and its delta.
        gesture: Gesture,
        /// Position of this event within the gesture.
        phase: Phase,
    },
    /// The user pressed the escape chord: local input is back, whatever the engine thinks.
    Escape,
}

/// A condition a backend copes with and the user may want to know about. Backends never print:
/// they report it through [`Warnings`] and whoever started them decides what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// Windows refused per-monitor DPI awareness (a window may already exist), so coordinates
    /// may be DPI-virtualised.
    DpiAwarenessRefused,
    /// Another X11 client holds the pointer or keyboard grab, so input stays local.
    GrabRefused,
    /// A portal task ended on its own after it had started.
    PortalEnded {
        /// Which task: `capture` or `injection`.
        what: &'static str,
        /// Why it ended.
        error: String,
    },
    /// The Wayland portal's restore token could not be saved, so the consent dialog returns.
    RestoreTokenNotSaved(String),
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DpiAwarenessRefused => {
                f.write_str("could not set per-monitor DPI awareness (a window may already exist)")
            }
            Self::GrabRefused => {
                f.write_str("X11: another client holds the pointer or keyboard grab; staying on local input")
            }
            Self::PortalEnded { what, error } => write!(f, "Wayland {what} ended: {error}"),
            Self::RestoreTokenNotSaved(e) => write!(f, "Wayland: could not save the restore token: {e}"),
        }
    }
}

/// Where a backend reports a [`Warning`], from whatever thread it happens on. The default
/// drops them; the binary that starts the backend installs one that logs.
#[derive(Clone, Default)]
pub struct Warnings(Option<Arc<dyn Fn(Warning) + Send + Sync>>);

impl Warnings {
    /// Warnings that go to `report`.
    pub fn new(report: impl Fn(Warning) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(report)))
    }

    /// Reports one warning.
    pub fn raise(&self, w: Warning) {
        if let Some(report) = &self.0 {
            report(w);
        }
    }
}

/// Where a capture backend delivers events: called on the capture thread for every event, so
/// a consumer can hand each straight to its own queue (a tokio channel's sync `send` works
/// from any thread) with no bridging thread in between.
#[derive(Clone)]
pub struct Sink {
    /// Takes each event.
    deliver: Arc<dyn Fn(Captured) -> Result<(), Captured> + Send + Sync>,
    /// Where the backend reports what it copes with.
    warnings: Warnings,
}

impl Sink {
    /// A sink that calls `deliver` for each event. It returns the event back as the error
    /// when nobody can take it any more; most backends then stop capturing.
    pub fn new(deliver: impl Fn(Captured) -> Result<(), Captured> + Send + Sync + 'static) -> Self {
        Self {
            deliver: Arc::new(deliver),
            warnings: Warnings::default(),
        }
    }

    /// This sink, with the backend's warnings going to `warnings`.
    #[must_use]
    pub fn with_warnings(mut self, warnings: Warnings) -> Self {
        self.warnings = warnings;
        self
    }

    /// Where the backend reports its warnings.
    pub fn warnings(&self) -> &Warnings {
        &self.warnings
    }

    /// A sink feeding a standard channel, and the receiver of what it is given.
    pub fn channel() -> (Self, Receiver<Captured>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self::new(move |c| tx.send(c).map_err(|e| e.0)), rx)
    }

    /// Delivers one event.
    ///
    /// # Errors
    ///
    /// Returns the event if the consumer is gone.
    pub fn send(&self, c: Captured) -> Result<(), Captured> {
        (self.deliver)(c)
    }
}

/// A value read from the OS that is trusted for a short time, so a hot path does not query
/// the OS for every event but a change it was never told about is still picked up.
#[derive(Debug, Clone, Copy)]
pub struct Stamped<T: Copy> {
    /// The value and when it was read; `None` before the first read or after [`Stamped::clear`].
    value: Option<(T, std::time::Instant)>,
}

impl<T: Copy> Stamped<T> {
    /// An empty cache.
    pub const fn new() -> Self {
        Self { value: None }
    }

    /// The value, if it was read less than `ttl` before `now`.
    pub fn get(&self, now: std::time::Instant, ttl: std::time::Duration) -> Option<T> {
        let (v, at) = self.value?;
        (now.saturating_duration_since(at) < ttl).then_some(v)
    }

    /// Remembers `v`, read at `now`.
    pub fn set(&mut self, v: T, now: std::time::Instant) {
        self.value = Some((v, now));
    }

    /// Forgets the value (the OS said it changed).
    pub fn clear(&mut self) {
        self.value = None;
    }
}

impl<T: Copy> Default for Stamped<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Overdesk code of the left Control key (HID usage), held while a pinch is replayed as a wheel.
pub const CTRL_KEY: u32 = 0xE0;

/// Overdesk code (HID usage) of Caps Lock.
pub const CAPS_CODE: u32 = 0x39;

/// Wheel units (1/120 notch) per thousandth of pinch scale, as a fraction: one notch per
/// 100 milli (10%), about one zoom step of a browser or editor per Ctrl+wheel notch.
const WHEEL_PER_MILLI: (i32, i32) = (120, 100);

/// Vertical wheel units that replay `gesture` as Ctrl+wheel on a PC: pinch out zooms in
/// (wheel up). `None` for gestures with no public PC equivalent (rotate, smart zoom).
pub fn pinch_wheel(gesture: Gesture) -> Option<i32> {
    match gesture {
        Gesture::Magnify { milli } => Some(milli.saturating_mul(WHEEL_PER_MILLI.0) / WHEEL_PER_MILLI.1),
        Gesture::Rotate { .. } | Gesture::SmartZoom => None,
    }
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
        fresh && code == ESC && either(CTRL_KEY, 0xE4) && either(0xE2, 0xE6) && either(0xE3, 0xE7)
    }
}

/// What an injector has pressed and not yet released, in press order, so it can release
/// everything on exit and know the modifiers still held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pressed<T>(
    /// The held items, oldest first, each at most once.
    Vec<T>,
);

impl<T> Default for Pressed<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T: Copy + PartialEq> Pressed<T> {
    /// Records a press or release of `item`; returns whether it was already held before
    /// (so a press that returns `true` is an auto-repeat).
    pub fn set(&mut self, item: T, down: bool) -> bool {
        let held = self.0.contains(&item);
        if down && !held {
            self.0.push(item);
        } else if !down {
            self.0.retain(|i| *i != item);
        }
        held
    }

    /// Whether `item` is held.
    pub fn contains(&self, item: &T) -> bool {
        self.0.contains(item)
    }

    /// The held items, oldest first.
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// A copy of the held items, oldest first: the order to release them in (reversed for
    /// keys) while the live set shrinks as each release is recorded.
    pub fn snapshot(&self) -> Vec<T> {
        self.0.clone()
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

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn a_stamped_value_expires() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let ttl = Duration::from_secs(1);
        let mut s = Stamped::new();
        assert_eq!(s.get(t0, ttl), None);
        s.set(5, t0);
        assert_eq!(s.get(t0 + ttl / 2, ttl), Some(5));
        assert_eq!(s.get(t0 + ttl, ttl), None);
        s.clear();
        assert_eq!(s.get(t0, ttl), None);
    }

    #[test]
    fn mac_pixels_become_smooth_wheel_units() {
        assert_eq!(to_units(-30, ScrollUnit::Pixel), -60, "half a notch down");
        assert_eq!(to_units(PX_PER_NOTCH, ScrollUnit::Pixel), 120, "one notch");
        assert_eq!(to_units(-120, ScrollUnit::Notch120), -120, "notches pass through");
    }

    #[test]
    fn notches_collect_and_a_direction_change_drops_the_rest() {
        let mut n = Notches::default();
        let px = ScrollUnit::Pixel;
        assert_eq!(n.feed(0, -20, px), (0, 0));
        assert_eq!(n.feed(0, -20, px), (0, 0));
        assert_eq!(n.feed(0, -20, px), (0, -1), "60 px is one notch down");
        assert_eq!(n.feed(90, 0, px), (1, 0));
        assert_eq!(n.feed(30, 0, px), (1, 0), "the half notch carried over");
        let mut n = Notches::default();
        let w = ScrollUnit::Notch120;
        assert_eq!(n.feed(0, 150, w), (0, 1)); // 30 left
        assert_eq!(n.feed(0, -40, w), (0, 0)); // direction change drops the 30
        assert_eq!(n.feed(0, -80, w), (0, -1));
        assert_eq!(n.feed(240, 0, w), (2, 0));
    }

    #[test]
    fn subpixel_accumulates_fractions_and_passes_whole_deltas() {
        let mut s = Subpixel::default();
        // Exactly representable fractions, so the expectation is exact.
        assert_eq!(s.feed(0.375, 0.0), (0, 0));
        assert_eq!(s.feed(0.375, 0.0), (0, 0));
        assert_eq!(s.feed(0.375, 0.0), (1, 0));
        assert_eq!(s.feed(-0.625, -0.625), (0, 0));
        assert_eq!(s.feed(-0.625, -0.625), (-1, -1));
        assert_eq!(Subpixel::default().feed(-42.0, 9.0), (-42, 9));
    }

    #[test]
    fn a_channel_sink_delivers_in_order_and_reports_a_gone_consumer() {
        let (sink, rx) = Sink::channel();
        assert!(sink.send(Captured::Escape).is_ok());
        assert!(sink.send(Captured::Key { code: 4, pressed: true }).is_ok());
        assert_eq!(rx.recv().unwrap(), Captured::Escape);
        assert_eq!(rx.recv().unwrap(), Captured::Key { code: 4, pressed: true });
        drop(rx);
        assert_eq!(sink.send(Captured::Escape), Err(Captured::Escape));
    }

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

    #[test]
    fn box_relative_moves_the_origin_to_the_box_corner() {
        assert_eq!(box_relative(&[]), vec![]);
        let two = [rect(-1280, -200, 1280, 1024), rect(0, 0, 1920, 1080)];
        assert_eq!(
            box_relative(&two),
            vec![rect(0, 0, 1280, 1024), rect(1280, 200, 1920, 1080)]
        );
    }

    #[test]
    fn bounding_box_spans_monitors_with_negative_origins() {
        assert_eq!(bounding_box([]), None);
        assert_eq!(bounding_box([rect(0, 0, 1920, 1080)]), Some(rect(0, 0, 1920, 1080)));
        // A second monitor to the left and above the main one.
        let two = [rect(0, 0, 1920, 1080), rect(-1280, -200, 1280, 1024)];
        assert_eq!(bounding_box(two), Some(rect(-1280, -200, 3200, 1280)));
        assert_eq!(to_local(-1280, -1280), 0);
    }

    #[test]
    fn pinch_maps_to_wheel_and_other_gestures_do_not() {
        assert_eq!(pinch_wheel(Gesture::Magnify { milli: 100 }), Some(120));
        assert_eq!(pinch_wheel(Gesture::Magnify { milli: -50 }), Some(-60));
        assert_eq!(pinch_wheel(Gesture::Rotate { millideg: 5000 }), None);
        assert_eq!(pinch_wheel(Gesture::SmartZoom), None);
    }
}
