// SPDX-License-Identifier: AGPL-3.0-only
//! Pure Wayland logic: session detection, barrier planning, the capture state machine
//! (enabled, activated, released) and pointer/scroll translation. No D-Bus or libei here, so
//! everything is tested on every OS; `wayland` does the I/O.

use std::ffi::OsString;
use std::path::PathBuf;

use overdesk_proto::{Button, Phase, ScrollUnit};

use super::events::Notches;
use super::keymap::keycode_to_code;
use crate::input::{Captured, EscapeChord};
use crate::mac::events::Subpixel;

/// How long an activation may stay unclaimed. The engine answers an edge crossing with
/// `set_swallow(true)` at once; no answer means it ignored the crossing (no neighbour on that
/// edge, or a local button is held), so capture is handed back.
pub const ARBITRATION_MS: u64 = 400;
/// Most discrete scroll steps one event may inject.
pub const MAX_STEPS: i32 = 64;
/// Relative motion that surely reaches the top-left corner, used to find the pointer when its
/// position is unknown.
const HOME: f64 = 32768.0;
/// Barrier ids are fixed per edge (index + 1) so an activation maps back to its edge.
const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

/// The display server family of this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A native Wayland session: use the portals.
    Wayland,
    /// Xorg: use XInput2 and XTest.
    X11,
}

/// Chooses the backend from `WAYLAND_DISPLAY` and `XDG_SESSION_TYPE`.
pub fn session_kind(wayland_display: Option<&str>, session_type: Option<&str>) -> SessionKind {
    if wayland_display.is_some_and(|v| !v.is_empty()) || session_type == Some("wayland") {
        SessionKind::Wayland
    } else {
        SessionKind::X11
    }
}

/// The error text when the portals fail on Wayland: names the cause and says why X11 is not
/// used instead (it only reaches XWayland windows).
pub fn portal_unavailable(reason: &str, display: Option<&str>) -> String {
    let x = if display.is_some_and(|d| !d.is_empty()) {
        "XWayland (DISPLAY) exists, but X11 input only reaches XWayland windows, so Overdesk does not fall back to it"
    } else {
        "there is no XWayland (DISPLAY) to fall back to"
    };
    format!(
        "Wayland input portals unavailable ({reason}); {x}. Needs xdg-desktop-portal with InputCapture and RemoteDesktop (GNOME 46+, Plasma 6.1+)"
    )
}

/// Where Overdesk keeps per-user files: `OVERDESK_HOME`, else `~/.overdesk` (as the daemon).
pub fn store_dir(overdesk_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    match overdesk_home {
        Some(d) => d.into(),
        None => PathBuf::from(home.unwrap_or_else(|| ".".into())).join(".overdesk"),
    }
}

/// A rectangle of the compositor's logical space (a portal zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in logical pixels.
    pub w: u32,
    /// Height in logical pixels.
    pub h: u32,
}

impl Zone {
    /// Right edge, exclusive.
    fn right(&self) -> i64 {
        i64::from(self.x) + i64::from(self.w)
    }

    /// Bottom edge, exclusive.
    fn bottom(&self) -> i64 {
        i64::from(self.y) + i64::from(self.h)
    }

    /// Whether the pixel at `p` is inside.
    fn contains(&self, p: (f64, f64)) -> bool {
        p.0 >= f64::from(self.x) && p.0 < self.right() as f64 && p.1 >= f64::from(self.y) && p.1 < self.bottom() as f64
    }

    /// The nearest point inside the zone.
    fn clamp(&self, p: (f64, f64)) -> (f64, f64) {
        (
            p.0.clamp(f64::from(self.x), (self.right() - 1) as f64),
            p.1.clamp(f64::from(self.y), (self.bottom() - 1) as f64),
        )
    }
}

/// The zone that holds the engine's screen: the one containing the origin, else the first.
pub fn primary_zone(zones: &[Zone]) -> Option<Zone> {
    zones
        .iter()
        .find(|z| z.contains((0.0, 0.0)))
        .or_else(|| zones.first())
        .copied()
}

/// Nearest point of any zone to `p`: `p` itself when inside one.
pub fn clamp_to_zones(zones: &[Zone], p: (f64, f64)) -> (f64, f64) {
    if zones.iter().any(|z| z.contains(p)) {
        return p;
    }
    let dist = |q: &(f64, f64)| (q.0 - p.0).powi(2) + (q.1 - p.1).powi(2);
    zones
        .iter()
        .map(|z| z.clamp(p))
        .min_by(|a, b| dist(a).total_cmp(&dist(b)))
        .unwrap_or(p)
}

/// A screen edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Left edge.
    Left,
    /// Right edge.
    Right,
    /// Top edge.
    Top,
    /// Bottom edge.
    Bottom,
}

impl Edge {
    /// The barrier id of this edge (1..=4, never zero as the portal requires).
    pub fn barrier_id(self) -> u32 {
        EDGES.iter().position(|e| *e == self).map_or(1, |i| i as u32 + 1)
    }

    /// The edge a barrier id belongs to.
    pub fn from_barrier_id(id: u32) -> Option<Self> {
        EDGES.get(usize::try_from(id).ok()?.checked_sub(1)?).copied()
    }

    /// Outward unit motion across this edge.
    fn outward(self) -> (i32, i32) {
        match self {
            Edge::Left => (-1, 0),
            Edge::Right => (1, 0),
            Edge::Top => (0, -1),
            Edge::Bottom => (0, 1),
        }
    }
}

/// One pointer barrier request: a line on a zone edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Barrier {
    /// Edge it guards; its id is [`Edge::barrier_id`].
    pub edge: Edge,
    /// Start x, inclusive.
    pub x1: i32,
    /// Start y, inclusive.
    pub y1: i32,
    /// End x, inclusive.
    pub x2: i32,
    /// End y, inclusive.
    pub y2: i32,
}

/// Barriers on the outer edges of `primary`. An edge another zone touches is internal (the
/// portal denies those), so it is skipped. Only the primary zone matters: it is the engine's
/// screen, and neighbours in the desk layout are peers beside it.
pub fn plan_barriers(primary: Zone, zones: &[Zone]) -> Vec<Barrier> {
    let (x, y) = (i64::from(primary.x), i64::from(primary.y));
    let (r, b) = (primary.right(), primary.bottom());
    let overlap = |a0: i64, a1: i64, b0: i64, b1: i64| a0 < b1 && b0 < a1;
    let touched = |edge: Edge| {
        zones.iter().filter(|z| **z != primary).any(|z| match edge {
            Edge::Left => z.right() == x && overlap(y, b, i64::from(z.y), z.bottom()),
            Edge::Right => i64::from(z.x) == r && overlap(y, b, i64::from(z.y), z.bottom()),
            Edge::Top => z.bottom() == y && overlap(x, r, i64::from(z.x), z.right()),
            Edge::Bottom => i64::from(z.y) == b && overlap(x, r, i64::from(z.x), z.right()),
        })
    };
    let (last_x, last_y) = ((r - 1) as i32, (b - 1) as i32);
    EDGES
        .iter()
        .filter(|e| !touched(**e))
        .map(|&edge| {
            // Right and bottom barriers sit on the line just past the zone, as the portal docs show.
            let (x1, y1, x2, y2) = match edge {
                Edge::Left => (primary.x, primary.y, primary.x, last_y),
                Edge::Right => (last_x + 1, primary.y, last_x + 1, last_y),
                Edge::Top => (primary.x, primary.y, last_x, primary.y),
                Edge::Bottom => (primary.x, last_y + 1, last_x, last_y + 1),
            };
            Barrier { edge, x1, y1, x2, y2 }
        })
        .collect()
}

/// The primary zone's edge nearest `p`; the fallback when the compositor cannot say which
/// barrier fired.
fn nearest_edge(z: Zone, p: (f64, f64)) -> Edge {
    let d = [
        p.0 - f64::from(z.x),
        z.right() as f64 - p.0,
        p.1 - f64::from(z.y),
        z.bottom() as f64 - p.1,
    ];
    let i = (0..4).min_by(|a, b| d[*a].total_cmp(&d[*b])).unwrap_or(0);
    EDGES[i]
}

/// Evdev code of a pointer button to the Overdesk button, `None` for others.
pub fn button_from_evdev(code: u32) -> Option<Button> {
    Some(match code {
        0x110 => Button::Left,
        0x111 => Button::Right,
        0x112 => Button::Middle,
        0x113 => Button::Back,
        0x114 => Button::Forward,
        _ => return None,
    })
}

/// The evdev code for an Overdesk button.
pub fn evdev_from_button(button: Button) -> u32 {
    match button {
        Button::Left => 0x110,
        Button::Right => 0x111,
        Button::Middle => 0x112,
        Button::Back => 0x113,
        Button::Forward => 0x114,
    }
}

/// An input to the capture machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Barriers are set and capture is enabled; these are the zones now.
    Armed(Vec<Zone>),
    /// The compositor started capture.
    Activated {
        /// Activation id to release with.
        id: u32,
        /// Cursor position at activation, compositor coordinates.
        pos: (f32, f32),
        /// The barrier that fired, if known.
        barrier: Option<u32>,
    },
    /// The compositor ended capture.
    Deactivated(Option<u32>),
    /// The compositor disabled the session: barriers must be set again.
    Disabled,
    /// The zones changed: barriers must be set again.
    ZonesChanged,
    /// The engine turned swallowing on or off.
    Swallow(bool),
    /// Periodic clock tick.
    Tick,
    /// Relative motion from EIS.
    Motion(f64, f64),
    /// A button edge from EIS (evdev code).
    Button(u32, bool),
    /// A key edge from EIS (evdev code).
    Key(u32, bool),
    /// Smooth scroll from EIS, positive down and right.
    ScrollSmooth(f64, f64),
    /// Discrete scroll from EIS in 1/120 notches, positive down and right.
    ScrollDiscrete(i32, i32),
    /// The portal session or EIS connection died.
    Failed,
}

/// What the I/O layer must do for the machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Send this to the engine.
    Emit(Captured),
    /// Release the activation with a cursor hint (compositor coordinates); `local` is the
    /// same point relative to the primary zone, for the injector.
    Release {
        /// Activation to end.
        id: u32,
        /// Hint in compositor coordinates.
        at: (f64, f64),
        /// Hint relative to the primary zone.
        local: (f64, f64),
    },
    /// Set barriers and enable capture again, then report [`Input::Armed`].
    Rearm,
}

/// A running activation.
#[derive(Debug)]
struct Active {
    /// Activation id.
    id: u32,
    /// Where to put the cursor on release, compositor coordinates.
    release_at: (f64, f64),
    /// Dead-reckoned cursor, compositor coordinates.
    pos: (f64, f64),
    /// Whether the engine claimed the activation.
    swallowing: bool,
    /// Clock reading at activation.
    since_ms: u64,
}

/// Where the capture is in its life.
#[derive(Debug)]
enum State {
    /// Barriers are not set; waiting for [`Input::Armed`].
    Disarmed,
    /// Armed and waiting for a barrier crossing.
    Idle,
    /// Capturing.
    Active(Active),
}

/// The capture state machine. Feed it [`Input`], perform the returned [`Out`].
#[derive(Debug)]
pub struct Machine {
    /// Current state.
    state: State,
    /// Zones from the last arming.
    zones: Vec<Zone>,
    /// Zone holding the engine's screen.
    primary: Option<Zone>,
    /// Escape chord detector, reset per activation.
    chord: EscapeChord,
    /// Rounds motion to whole pixels.
    motion: Subpixel,
    /// Rounds smooth scroll to whole pixels.
    scroll: Subpixel,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            state: State::Disarmed,
            zones: Vec::new(),
            primary: None,
            chord: EscapeChord::default(),
            motion: Subpixel::default(),
            scroll: Subpixel::default(),
        }
    }
}

impl Machine {
    /// Size of the engine's screen, once armed.
    pub fn primary_size(&self) -> Option<(u32, u32)> {
        self.primary.map(|z| (z.w, z.h))
    }

    /// Whether a capture is running.
    pub fn is_active(&self) -> bool {
        matches!(self.state, State::Active(_))
    }

    /// Ends the activation (if any) and returns the release request.
    fn release(&mut self) -> Option<Out> {
        let State::Active(a) = std::mem::replace(&mut self.state, State::Idle) else {
            return None;
        };
        let p = self.primary?;
        Some(Out::Release {
            id: a.id,
            at: a.release_at,
            local: (a.release_at.0 - f64::from(p.x), a.release_at.1 - f64::from(p.y)),
        })
    }

    /// Position relative to the primary zone, as the engine wants it.
    fn local(&self, p: (f64, f64)) -> (i32, i32) {
        let z = self.primary.unwrap_or(Zone { x: 0, y: 0, w: 0, h: 0 });
        (
            (p.0 - f64::from(z.x)).round() as i32,
            (p.1 - f64::from(z.y)).round() as i32,
        )
    }

    /// Handles one input at clock time `now_ms`.
    pub fn step(&mut self, now_ms: u64, input: Input) -> Vec<Out> {
        let mut out = Vec::new();
        match input {
            Input::Armed(zones) => {
                self.primary = primary_zone(&zones);
                self.zones = zones;
                if !self.is_active() {
                    self.state = State::Idle;
                }
            }
            Input::Activated { id, pos, barrier } => self.activate(now_ms, id, pos, barrier, &mut out),
            Input::Deactivated(id) => {
                if let State::Active(a) = &self.state
                    && id.is_none_or(|i| i == a.id)
                {
                    // The compositor ended it, not us: the engine must come back local.
                    let swallowing = a.swallowing;
                    self.state = State::Idle;
                    if swallowing {
                        out.push(Out::Emit(Captured::Escape));
                    }
                }
            }
            Input::Disabled | Input::ZonesChanged => {
                if self.is_active() {
                    out.push(Out::Emit(Captured::Escape));
                }
                self.state = State::Disarmed;
                out.push(Out::Rearm);
            }
            Input::Swallow(true) => match &mut self.state {
                State::Active(a) => a.swallowing = true,
                // Nothing is captured, so the engine cannot drive a peer: send it back.
                _ => out.push(Out::Emit(Captured::Escape)),
            },
            Input::Swallow(false) => out.extend(self.release()),
            Input::Tick => {
                if let State::Active(a) = &self.state
                    && !a.swallowing
                    && now_ms.saturating_sub(a.since_ms) >= ARBITRATION_MS
                {
                    out.extend(self.release());
                }
            }
            Input::Failed => {
                self.state = State::Disarmed;
                out.push(Out::Emit(Captured::Escape));
            }
            captured => self.captured(captured, &mut out),
        }
        out
    }

    /// Starts an activation: reports the crossing to the engine as an outward push at the
    /// edge, which is what a local cursor stopping at that edge looks like to it.
    fn activate(&mut self, now_ms: u64, id: u32, pos: (f32, f32), barrier: Option<u32>, out: &mut Vec<Out>) {
        let (Some(z), State::Idle | State::Active(_)) = (self.primary, &self.state) else {
            return;
        };
        let p = (f64::from(pos.0), f64::from(pos.1));
        let edge = barrier
            .and_then(Edge::from_barrier_id)
            .unwrap_or_else(|| nearest_edge(z, p));
        let (cx, cy) = z.clamp(p);
        let (ox, oy) = edge.outward();
        let entry = match edge {
            Edge::Left => (f64::from(z.x), cy),
            Edge::Right => ((z.right() - 1) as f64, cy),
            Edge::Top => (cx, f64::from(z.y)),
            Edge::Bottom => (cx, (z.bottom() - 1) as f64),
        };
        // Release one pixel inside the edge so the cursor does not re-trigger the barrier.
        let release_at = (entry.0 - f64::from(ox), entry.1 - f64::from(oy));
        self.chord = EscapeChord::default();
        self.motion = Subpixel::default();
        self.scroll = Subpixel::default();
        let (x, y) = self.local(entry);
        self.state = State::Active(Active {
            id,
            release_at,
            pos: entry,
            swallowing: false,
            since_ms: now_ms,
        });
        out.push(Out::Emit(Captured::Moved { dx: ox, dy: oy, x, y }));
    }

    /// Handles EIS input events; only meaningful while active.
    fn captured(&mut self, input: Input, out: &mut Vec<Out>) {
        let State::Active(a) = &mut self.state else {
            return;
        };
        let forward = a.swallowing;
        match input {
            Input::Motion(dx, dy) => {
                let (ix, iy) = self.motion.feed(dx, dy);
                a.pos = clamp_to_zones(&self.zones, (a.pos.0 + dx, a.pos.1 + dy));
                let pos = a.pos;
                if forward && (ix, iy) != (0, 0) {
                    let (x, y) = self.local(pos);
                    out.push(Out::Emit(Captured::Moved { dx: ix, dy: iy, x, y }));
                }
            }
            Input::Button(code, pressed) => {
                if let (true, Some(button)) = (forward, button_from_evdev(code)) {
                    out.push(Out::Emit(Captured::Button { button, pressed }));
                }
            }
            Input::Key(evdev, pressed) => {
                // X11 keycodes are evdev + 8, which the shared table is keyed by.
                let Some(code) = keycode_to_code(evdev + 8) else {
                    return;
                };
                if self.chord.key(code, pressed) {
                    out.extend(self.release());
                    out.push(Out::Emit(Captured::Escape));
                } else if forward {
                    out.push(Out::Emit(Captured::Key { code, pressed }));
                }
            }
            Input::ScrollSmooth(dx, dy) => {
                let (sx, sy) = self.scroll.feed(dx, -dy);
                if forward && (sx, sy) != (0, 0) {
                    out.push(Out::Emit(Captured::Scroll {
                        dx: sx,
                        dy: sy,
                        unit: ScrollUnit::Pixel,
                        phase: Phase::Changed,
                    }));
                }
            }
            Input::ScrollDiscrete(dx, dy) if forward => {
                out.push(Out::Emit(Captured::Scroll {
                    dx,
                    dy: dy.saturating_neg(),
                    unit: ScrollUnit::Notch120,
                    phase: Phase::None,
                }));
            }
            _ => {}
        }
    }
}

/// Tracks where the injected pointer is. The RemoteDesktop path only moves it relatively, so
/// absolute targets from the engine become deltas from the last known position.
#[derive(Debug, Default)]
pub struct Pointer {
    /// Last known position relative to the primary zone, `None` until learned.
    pos: Option<(f64, f64)>,
}

impl Pointer {
    /// Records a position learned elsewhere (a capture release hint).
    pub fn set(&mut self, p: (f64, f64)) {
        self.pos = Some(p);
    }

    /// The last known position.
    pub fn get(&self) -> Option<(f64, f64)> {
        self.pos
    }

    /// Relative moves that bring the pointer to `(x, y)`. With no known position it first
    /// homes to the top-left corner (the compositor clamps), then moves from there.
    /// shortcut: assumes the primary zone starts at the compositor origin; a hand test on
    /// multi-monitor layouts decides whether it needs the zone offset.
    pub fn move_to(&mut self, x: f64, y: f64) -> Vec<(f64, f64)> {
        let (x, y) = (x.max(0.0), y.max(0.0));
        let mut moves = Vec::new();
        let from = self.pos.unwrap_or_else(|| {
            moves.push((-HOME, -HOME));
            (0.0, 0.0)
        });
        if (x - from.0, y - from.1) != (0.0, 0.0) {
            moves.push((x - from.0, y - from.1));
        }
        self.pos = Some((x, y));
        moves
    }
}

/// One scroll request for the RemoteDesktop portal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollOut {
    /// Smooth scroll in pixels, positive down and right.
    Smooth(f64, f64),
    /// Wheel steps on one axis, positive down or right.
    Steps {
        /// `true` for the horizontal axis.
        horizontal: bool,
        /// Whole notches.
        steps: i32,
    },
}

/// Translates an Overdesk scroll (positive up and right) into portal requests: pixels stay
/// smooth, notches become whole discrete steps with the remainder carried in `notches`.
pub fn scroll_out(notches: &mut Notches, dx: i32, dy: i32, unit: ScrollUnit) -> Vec<ScrollOut> {
    match unit {
        ScrollUnit::Pixel => vec![ScrollOut::Smooth(f64::from(dx), -f64::from(dy))],
        ScrollUnit::Notch120 => {
            let (cx, cy) = notches.feed(dx, dy, unit);
            [(true, cx), (false, -cy)]
                .into_iter()
                .filter(|(_, s)| *s != 0)
                .map(|(horizontal, s)| ScrollOut::Steps {
                    horizontal,
                    steps: s.clamp(-MAX_STEPS, MAX_STEPS),
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Z: Zone = Zone {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    fn armed(zones: &[Zone]) -> Machine {
        let mut m = Machine::default();
        m.step(0, Input::Armed(zones.to_vec()));
        m
    }

    fn cross(m: &mut Machine, now: u64, edge: Edge, pos: (f32, f32)) -> Vec<Out> {
        m.step(
            now,
            Input::Activated {
                id: 7,
                pos,
                barrier: Some(edge.barrier_id()),
            },
        )
    }

    #[test]
    fn session_detection_and_error_text() {
        assert_eq!(session_kind(Some("wayland-0"), None), SessionKind::Wayland);
        assert_eq!(session_kind(None, Some("wayland")), SessionKind::Wayland);
        assert_eq!(session_kind(Some(""), Some("x11")), SessionKind::X11);
        assert!(portal_unavailable("no InputCapture", Some(":0")).contains("only reaches XWayland windows"));
        assert!(portal_unavailable("no InputCapture", None).contains("no XWayland"));
    }

    #[test]
    fn store_dir_prefers_overdesk_home() {
        assert_eq!(store_dir(Some("/o".into()), Some("/h".into())), PathBuf::from("/o"));
        assert_eq!(store_dir(None, Some("/h".into())), PathBuf::from("/h/.overdesk"));
    }

    #[test]
    fn barrier_ids_map_back_to_edges() {
        for e in EDGES {
            assert_eq!(Edge::from_barrier_id(e.barrier_id()), Some(e));
        }
        assert_eq!(Edge::from_barrier_id(0), None);
        assert_eq!(Edge::from_barrier_id(5), None);
    }

    #[test]
    fn barriers_skip_edges_shared_with_another_monitor() {
        let right = Zone {
            x: 1920,
            y: 0,
            w: 1280,
            h: 1024,
        };
        let b = plan_barriers(Z, &[Z, right]);
        assert_eq!(b.len(), 3);
        assert!(b.iter().all(|b| b.edge != Edge::Right));
        let all = plan_barriers(Z, &[Z]);
        assert_eq!(all.len(), 4);
        let r = all.iter().find(|b| b.edge == Edge::Right).unwrap();
        assert_eq!((r.x1, r.y1, r.x2, r.y2), (1920, 0, 1920, 1079));
        let bt = all.iter().find(|b| b.edge == Edge::Bottom).unwrap();
        assert_eq!((bt.x1, bt.y1, bt.x2, bt.y2), (0, 1080, 1919, 1080));
    }

    #[test]
    fn primary_is_the_zone_at_the_origin() {
        let other = Zone {
            x: -1000,
            y: 0,
            w: 1000,
            h: 800,
        };
        assert_eq!(primary_zone(&[other, Z]), Some(Z));
        assert_eq!(primary_zone(&[other]), Some(other));
        assert_eq!(primary_zone(&[]), None);
    }

    #[test]
    fn activation_reports_an_outward_push_at_the_edge() {
        let mut m = armed(&[Z]);
        let out = cross(&mut m, 10, Edge::Right, (1920.0, 400.4));
        assert_eq!(
            out,
            vec![Out::Emit(Captured::Moved {
                dx: 1,
                dy: 0,
                x: 1919,
                y: 400
            })]
        );
        let out = cross(&mut armed(&[Z]), 0, Edge::Top, (300.0, 0.0));
        assert_eq!(
            out,
            vec![Out::Emit(Captured::Moved {
                dx: 0,
                dy: -1,
                x: 300,
                y: 0
            })]
        );
    }

    #[test]
    fn unknown_barrier_falls_back_to_the_nearest_edge() {
        let mut m = armed(&[Z]);
        let out = m.step(
            0,
            Input::Activated {
                id: 1,
                pos: (3.0, 500.0),
                barrier: None,
            },
        );
        assert_eq!(
            out,
            vec![Out::Emit(Captured::Moved {
                dx: -1,
                dy: 0,
                x: 0,
                y: 500
            })]
        );
    }

    #[test]
    fn unclaimed_activation_is_released_after_the_arbitration_window() {
        let mut m = armed(&[Z]);
        cross(&mut m, 100, Edge::Right, (1920.0, 540.0));
        assert!(m.step(100 + ARBITRATION_MS - 1, Input::Tick).is_empty());
        let out = m.step(100 + ARBITRATION_MS, Input::Tick);
        assert_eq!(
            out,
            vec![Out::Release {
                id: 7,
                at: (1918.0, 540.0),
                local: (1918.0, 540.0)
            }]
        );
        assert!(!m.is_active());
    }

    #[test]
    fn swallow_claims_the_activation_and_forwards_input() {
        let mut m = armed(&[Z]);
        cross(&mut m, 0, Edge::Right, (1920.0, 540.0));
        assert!(m.step(1, Input::Swallow(true)).is_empty());
        assert!(m.step(9999, Input::Tick).is_empty(), "claimed, never timed out");
        // Entry is at x = 1919; motion to the right is clamped to the zone for x, y moves on.
        assert_eq!(
            m.step(2, Input::Motion(5.0, -3.0)),
            vec![Out::Emit(Captured::Moved {
                dx: 5,
                dy: -3,
                x: 1919,
                y: 537
            })]
        );
        assert_eq!(
            m.step(3, Input::Button(0x110, true)),
            vec![Out::Emit(Captured::Button {
                button: Button::Left,
                pressed: true
            })]
        );
        assert_eq!(
            m.step(3, Input::Key(30, true)),
            vec![Out::Emit(Captured::Key {
                code: 0x04,
                pressed: true
            })]
        );
        let out = m.step(4, Input::Swallow(false));
        assert!(matches!(out.as_slice(), [Out::Release { id: 7, .. }]));
        assert!(m.step(5, Input::Motion(1.0, 1.0)).is_empty(), "idle input is dropped");
    }

    #[test]
    fn input_before_the_engine_claims_is_not_forwarded() {
        let mut m = armed(&[Z]);
        cross(&mut m, 0, Edge::Left, (0.0, 10.0));
        assert!(m.step(1, Input::Motion(-4.0, 0.0)).is_empty());
        assert!(m.step(1, Input::Key(30, true)).is_empty());
    }

    #[test]
    fn dead_reckoning_crosses_zones_and_clamps_outside() {
        let right = Zone {
            x: 1920,
            y: 0,
            w: 1280,
            h: 1024,
        };
        let zs = [Z, right];
        assert_eq!(clamp_to_zones(&zs, (2000.0, 10.0)), (2000.0, 10.0));
        assert_eq!(clamp_to_zones(&zs, (5000.0, 2000.0)), (3199.0, 1023.0));
        assert_eq!(clamp_to_zones(&zs, (-9.0, 5.0)), (0.0, 5.0));
    }

    #[test]
    fn escape_chord_from_eis_keys_releases_and_tells_the_engine() {
        let mut m = armed(&[Z]);
        cross(&mut m, 0, Edge::Right, (1920.0, 5.0));
        m.step(1, Input::Swallow(true));
        // Evdev: left ctrl 29, left alt 56, left meta 125, escape 1.
        for k in [29, 56, 125] {
            m.step(2, Input::Key(k, true));
        }
        let out = m.step(3, Input::Key(1, true));
        assert!(matches!(
            out.as_slice(),
            [Out::Release { .. }, Out::Emit(Captured::Escape)]
        ));
        assert!(!m.is_active());
    }

    #[test]
    fn chord_works_even_before_the_engine_claims() {
        let mut m = armed(&[Z]);
        cross(&mut m, 0, Edge::Right, (1920.0, 5.0));
        for k in [29, 56, 125] {
            m.step(2, Input::Key(k, true));
        }
        let out = m.step(3, Input::Key(1, true));
        assert!(matches!(
            out.as_slice(),
            [Out::Release { .. }, Out::Emit(Captured::Escape)]
        ));
    }

    #[test]
    fn compositor_side_end_returns_the_engine_to_local() {
        let mut m = armed(&[Z]);
        cross(&mut m, 0, Edge::Right, (1920.0, 5.0));
        m.step(1, Input::Swallow(true));
        assert_eq!(
            m.step(2, Input::Deactivated(Some(7))),
            vec![Out::Emit(Captured::Escape)]
        );
        // A stale id must not end a newer activation.
        let mut m = armed(&[Z]);
        cross(&mut m, 0, Edge::Right, (1920.0, 5.0));
        assert!(m.step(1, Input::Deactivated(Some(99))).is_empty());
        assert!(m.is_active());
    }

    #[test]
    fn disabled_and_zone_changes_escape_and_rearm() {
        for ev in [Input::Disabled, Input::ZonesChanged] {
            let mut m = armed(&[Z]);
            cross(&mut m, 0, Edge::Right, (1920.0, 5.0));
            m.step(1, Input::Swallow(true));
            assert_eq!(m.step(2, ev.clone()), vec![Out::Emit(Captured::Escape), Out::Rearm]);
            let stray = Input::Activated {
                id: 8,
                pos: (0.0, 0.0),
                barrier: Some(1),
            };
            assert!(m.step(3, stray).is_empty(), "no activation while disarmed");
            let bigger = Zone {
                x: 0,
                y: 0,
                w: 2560,
                h: 1440,
            };
            m.step(4, Input::Armed(vec![bigger]));
            assert_eq!(m.primary_size(), Some((2560, 1440)));
            assert!(!cross(&mut m, 5, Edge::Left, (0.0, 1.0)).is_empty());
        }
        let mut idle = armed(&[Z]);
        assert_eq!(idle.step(0, Input::Disabled), vec![Out::Rearm]);
    }

    #[test]
    fn failure_always_sends_escape_and_swallow_without_capture_does_too() {
        let mut m = armed(&[Z]);
        assert_eq!(m.step(0, Input::Failed), vec![Out::Emit(Captured::Escape)]);
        let mut m = armed(&[Z]);
        assert_eq!(m.step(0, Input::Swallow(true)), vec![Out::Emit(Captured::Escape)]);
    }

    #[test]
    fn scroll_from_eis_flips_to_up_positive() {
        let mut m = armed(&[Z]);
        cross(&mut m, 0, Edge::Right, (1920.0, 5.0));
        m.step(1, Input::Swallow(true));
        assert_eq!(
            m.step(2, Input::ScrollDiscrete(0, 120)),
            vec![Out::Emit(Captured::Scroll {
                dx: 0,
                dy: -120,
                unit: ScrollUnit::Notch120,
                phase: Phase::None
            })]
        );
        assert_eq!(
            m.step(2, Input::ScrollSmooth(4.0, 10.0)),
            vec![Out::Emit(Captured::Scroll {
                dx: 4,
                dy: -10,
                unit: ScrollUnit::Pixel,
                phase: Phase::Changed
            })]
        );
    }

    #[test]
    fn pointer_homes_once_then_moves_relatively() {
        let mut p = Pointer::default();
        assert_eq!(p.move_to(100.0, 50.0), vec![(-HOME, -HOME), (100.0, 50.0)]);
        assert_eq!(p.move_to(90.0, 60.0), vec![(-10.0, 10.0)]);
        assert!(p.move_to(90.0, 60.0).is_empty());
        let mut known = Pointer::default();
        known.set((10.0, 10.0));
        assert_eq!(known.move_to(15.0, 10.0), vec![(5.0, 0.0)]);
        assert_eq!(known.get(), Some((15.0, 10.0)));
    }

    #[test]
    fn scroll_out_maps_units_and_directions() {
        let mut n = Notches::default();
        assert_eq!(
            scroll_out(&mut n, 5, 10, ScrollUnit::Pixel),
            vec![ScrollOut::Smooth(5.0, -10.0)]
        );
        assert_eq!(
            scroll_out(&mut n, 120, 240, ScrollUnit::Notch120),
            vec![
                ScrollOut::Steps {
                    horizontal: true,
                    steps: 1
                },
                ScrollOut::Steps {
                    horizontal: false,
                    steps: -2
                }
            ]
        );
        assert!(scroll_out(&mut n, 0, 60, ScrollUnit::Notch120).is_empty());
        assert_eq!(
            scroll_out(&mut n, 0, 60, ScrollUnit::Notch120),
            vec![ScrollOut::Steps {
                horizontal: false,
                steps: -1
            }]
        );
    }

    #[test]
    fn buttons_round_trip() {
        for b in [
            Button::Left,
            Button::Right,
            Button::Middle,
            Button::Back,
            Button::Forward,
        ] {
            assert_eq!(button_from_evdev(evdev_from_button(b)), Some(b));
        }
        assert_eq!(button_from_evdev(1), None);
    }
}
