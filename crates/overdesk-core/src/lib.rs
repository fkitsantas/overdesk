// SPDX-License-Identifier: AGPL-3.0-only
//! Platform-free Overdesk logic: screen layout, cursor handoff, held-key tracking.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
pub mod engine;

pub use overdesk_proto::{Desk, DeskScreen, Os, PeerId, Rect};

/// A named screen placed in the shared logical plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    /// Name, unique within a [`Layout`].
    pub name: String,
    /// Position and size in global logical coordinates.
    pub rect: Rect,
}

/// Why a set of screens does not form a valid [`Layout`]. Variants carry screen names.
#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// The layout has no screens.
    Empty,
    /// The named screen has zero width or height.
    ZeroSize(String),
    /// The named screen's size or far edge does not fit in `i32` coordinates.
    OutOfRange(String),
    /// Two screens share this name.
    DuplicateName(String),
    /// The two named screens overlap; the first comes earlier in the list.
    Overlap(String, String),
    /// The Desk has no screen for this device (reported by `Engine::set_desk`).
    Unplaced,
    /// Two screens of the Desk belong to the same peer; the payload names the later one.
    /// A device has one screen, so the document is ambiguous about where its cursor is.
    DuplicatePeer(String),
}

/// A plain sentence for a person, naming the screens involved.
impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("the layout has no screens"),
            Self::ZeroSize(n) => write!(f, "the screen \"{n}\" has no width or height"),
            Self::OutOfRange(n) => write!(f, "the screen \"{n}\" is too large"),
            Self::DuplicateName(n) => write!(f, "two screens are both named \"{n}\""),
            Self::Overlap(a, b) => write!(f, "the screens \"{a}\" and \"{b}\" overlap"),
            Self::Unplaced => f.write_str("this computer has no screen in the layout"),
            Self::DuplicatePeer(n) => write!(f, "the computer of the screen \"{n}\" appears twice in the layout"),
        }
    }
}

impl std::error::Error for LayoutError {}

/// A validated arrangement of screens: non-empty, uniquely named, non-zero sized,
/// non-overlapping, and with every coordinate representable as `i32`.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Screens in input order; their index is the screen id.
    screens: Vec<Screen>,
}

impl Layout {
    /// Validates `screens` and builds a layout, reporting the first violation found.
    /// The overlap check is O(n²); callers bound `n` for untrusted input (see [`MAX_SCREENS`]).
    pub fn new(screens: Vec<Screen>) -> Result<Self, LayoutError> {
        if screens.is_empty() {
            return Err(LayoutError::Empty);
        }
        for (i, a) in screens.iter().enumerate() {
            if a.rect.w == 0 || a.rect.h == 0 {
                return Err(LayoutError::ZeroSize(a.name.clone()));
            }
            // Bounding size and far edges keeps every global and local coordinate within i32.
            let too_big = a.rect.w > i32::MAX as u32 || a.rect.h > i32::MAX as u32;
            if too_big || a.rect.right() > i32::MAX as i64 || a.rect.bottom() > i32::MAX as i64 {
                return Err(LayoutError::OutOfRange(a.name.clone()));
            }
            for b in &screens[..i] {
                if b.name == a.name {
                    return Err(LayoutError::DuplicateName(a.name.clone()));
                }
                if b.rect.overlaps(&a.rect) {
                    return Err(LayoutError::Overlap(b.name.clone(), a.name.clone()));
                }
            }
        }
        Ok(Self { screens })
    }

    /// Index of the screen containing the global point `p`, if any.
    fn find(&self, p: (i64, i64)) -> Option<usize> {
        self.screens.iter().position(|s| s.rect.contains(p))
    }

    /// Screens in their original order, indexed by screen id.
    pub fn screens(&self) -> &[Screen] {
        &self.screens
    }
}

/// Whether `n` lies directly across `cur`'s edge in direction (`sx`, `sy`), sharing part of it.
fn touches(cur: &Rect, n: &Rect, sx: i64, sy: i64) -> bool {
    let overlap = |a0: i64, a1: i64, b0: i64, b1: i64| a0 < b1 && b0 < a1;
    match (sx, sy) {
        (1, 0) => n.x as i64 == cur.right() && overlap(cur.y as i64, cur.bottom(), n.y as i64, n.bottom()),
        (-1, 0) => n.right() == cur.x as i64 && overlap(cur.y as i64, cur.bottom(), n.y as i64, n.bottom()),
        (0, 1) => n.y as i64 == cur.bottom() && overlap(cur.x as i64, cur.right(), n.x as i64, n.right()),
        (0, -1) => n.bottom() == cur.y as i64 && overlap(cur.x as i64, cur.right(), n.x as i64, n.right()),
        _ => false,
    }
}

/// A device's screen is its bounding box on the Desk, but its monitors may cover only part
/// of it (an L-shaped layout: a laptop beside a taller display leaves a hole). `monitors`
/// are the real rectangles, local to the box. These two functions are the whole hole rule.
///
/// The nearest pixel of any monitor to `(x, y)`: the point itself when it is on a monitor.
/// Where the cursor enters from a peer is a point of the box, which may fall in a hole the
/// OS would clamp somewhere arbitrary. `(x, y)` is returned unchanged without monitors.
pub(crate) fn snap_to_monitors(monitors: &[Rect], x: i32, y: i32) -> (i32, i32) {
    let p = (i64::from(x), i64::from(y));
    monitors
        .iter()
        .filter(|m| m.w > 0 && m.h > 0)
        .map(|m| m.clamp(p))
        // u128 of abs_diff: coordinates near i32::MIN/MAX would overflow i64 squares.
        .min_by_key(|q| u128::from(q.0.abs_diff(p.0)).pow(2) + u128::from(q.1.abs_diff(p.1)).pow(2))
        .map_or((x, y), |q| (q.0 as i32, q.1 as i32))
}

/// How far a local cursor at `pos` pushed by `delta` leaves the `size` box, as the delta
/// to feed `Switcher::move_by`, per axis (0 = stays).
///
/// Rule: moving off the visible edge of the monitor under the cursor, towards the box
/// edge on that side, leaves the screen when no monitor lies ahead in that direction on
/// the cursor's row (or column). The cursor is carried along that line to the box edge,
/// so the position along the other axis is kept and the neighbour is picked as usual.
/// An edge facing a hole therefore counts as the box edge extended; an edge facing
/// another monitor does not. A single monitor equal to the box gives the plain rule:
/// leave when at the box edge and pushing out.
///
/// No monitors (an empty slice) means the box itself is the one monitor.
pub fn exit_push(monitors: &[Rect], size: (u32, u32), pos: (i32, i32), delta: (i32, i32)) -> (i32, i32) {
    let whole = [Rect::whole(size.0, size.1)];
    let monitors = if monitors.is_empty() { &whole[..] } else { monitors };
    let to_i32 = |v: i64| i32::try_from(v).unwrap_or(i32::MAX);
    let along = |horizontal: bool| {
        let (d, extent, a, c) = if horizontal {
            (delta.0, size.0, pos.0, pos.1)
        } else {
            (delta.1, size.1, pos.1, pos.0)
        };
        let (a, c, extent) = (i64::from(a), i64::from(c), i64::from(extent));
        // A monitor's span (lo..hi) along the push and (clo..chi) across it.
        let blocked = monitors.iter().any(|m| {
            let ((lo, hi), (clo, chi)) = if horizontal {
                ((i64::from(m.x), m.right()), (i64::from(m.y), m.bottom()))
            } else {
                ((i64::from(m.y), m.bottom()), (i64::from(m.x), m.right()))
            };
            (clo..chi).contains(&c) && if d > 0 { hi > a + 1 } else { lo < a }
        });
        match d {
            0 => 0,
            _ if blocked => 0,
            // Carry the cursor on to the box edge, then the push itself.
            _ if d > 0 => d.saturating_add(to_i32((extent - 1 - a).max(0))),
            _ => d.saturating_sub(to_i32(a.max(0))),
        }
    };
    (along(true), along(false))
}

/// Result of a relative cursor move. Coordinates are local to `screen` / `to`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Move {
    /// The cursor stayed on `screen`, possibly clamped against an edge with no neighbour.
    Within {
        /// Index of the screen that still holds the cursor.
        screen: usize,
        /// New x, local to `screen`.
        x: i32,
        /// New y, local to `screen`.
        y: i32,
    },
    /// The cursor crossed an edge from screen `from` onto screen `to`.
    Switched {
        /// Index of the screen the cursor left.
        from: usize,
        /// Index of the screen the cursor entered.
        to: usize,
        /// Entry x, local to `to`.
        x: i32,
        /// Entry y, local to `to`.
        y: i32,
    },
}

/// Tracks which screen owns the cursor, driven by relative mouse deltas.
#[derive(Debug)]
pub(crate) struct Switcher {
    /// Screens the cursor can move across.
    layout: Layout,
    /// Index of the screen that holds the cursor.
    active: usize,
    /// Cursor position in global logical coordinates; always inside `active`.
    pos: (i32, i32),
}

impl Switcher {
    /// Starts at the center of screen `start`; `None` if no such screen.
    pub fn new(layout: Layout, start: usize) -> Option<Self> {
        let r = layout.screens.get(start)?.rect;
        let pos = (r.x + (r.w / 2) as i32, r.y + (r.h / 2) as i32);
        Some(Self {
            layout,
            active: start,
            pos,
        })
    }

    /// The layout the cursor moves across.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Re-syncs to where the OS says the cursor is (`x`, `y` local to `screen`), clamped.
    /// Panics if `screen` is not an index into the layout.
    pub fn set_local(&mut self, screen: usize, x: i32, y: i32) {
        let r = self.layout.screens[screen].rect;
        let p = r.clamp((r.x as i64 + x as i64, r.y as i64 + y as i64));
        self.active = screen;
        self.pos = (p.0 as i32, p.1 as i32);
    }

    /// `(screen, x, y)` with `x`, `y` local to that screen.
    pub fn local(&self) -> (usize, i32, i32) {
        let r = self.layout.screens[self.active].rect;
        (self.active, self.pos.0 - r.x, self.pos.1 - r.y)
    }

    /// Moves the cursor by a relative delta in logical pixels. Crossing an edge that
    /// touches another screen switches to it; an edge with no neighbour clamps the cursor.
    pub fn move_by(&mut self, dx: i32, dy: i32) -> Move {
        let from = self.active;
        let target = (self.pos.0 as i64 + dx as i64, self.pos.1 as i64 + dy as i64);
        let cur = self.layout.screens[from].rect;
        if cur.contains(target) {
            return self.settle(target, |screen, x, y| Move::Within { screen, x, y });
        }
        let edge = cur.clamp(target);
        // Exit direction per axis: -1, 0 or 1, from how far the target overshot the edge.
        let (sx, sy) = ((target.0 - edge.0).signum(), (target.1 - edge.1).signum());
        // Straight exit with exactly one screen along that edge: map the position along
        // the edge proportionally, so screens of different sizes have no dead zones.
        if (sx == 0) != (sy == 0) {
            let mut along = self
                .layout
                .screens
                .iter()
                .enumerate()
                .filter(|(i, s)| *i != from && touches(&cur, &s.rect, sx, sy));
            // Taking two items tells "exactly one neighbour" apart from "several".
            if let (Some((to, n)), None) = (along.next(), along.next()) {
                let n = n.rect;
                let landing = if sx != 0 {
                    let y = n.y as i64 + (edge.1 - cur.y as i64) * n.h as i64 / cur.h as i64;
                    (target.0.clamp(n.x as i64, n.right() - 1), y)
                } else {
                    let x = n.x as i64 + (edge.0 - cur.x as i64) * n.w as i64 / cur.w as i64;
                    (x, target.1.clamp(n.y as i64, n.bottom() - 1))
                };
                self.active = to;
                return self.settle(landing, |to, x, y| Move::Switched { from, to, x, y });
            }
        }
        // Otherwise probe the pixel just past the edge; for corner exits try each axis alone.
        for (px, py) in [(sx, sy), (sx, 0), (0, sy)] {
            if (px, py) == (0, 0) {
                continue;
            }
            if let Some(to) = self.layout.find((edge.0 + px, edge.1 + py)) {
                self.active = to;
                let landing = self.layout.screens[to].rect.clamp(target);
                return self.settle(landing, |to, x, y| Move::Switched { from, to, x, y });
            }
        }
        self.settle(edge, |screen, x, y| Move::Within { screen, x, y })
    }

    /// Stores `p` (already inside the active screen) and reports it in local coordinates.
    fn settle(&mut self, p: (i64, i64), make: impl FnOnce(usize, i32, i32) -> Move) -> Move {
        self.pos = (p.0 as i32, p.1 as i32);
        let r = self.layout.screens[self.active].rect;
        make(self.active, self.pos.0 - r.x, self.pos.1 - r.y)
    }
}

/// Builds the validated layout a Desk document describes: a valid [`Layout`] in which each
/// peer owns at most one screen.
///
/// # Errors
/// Any [`Layout::new`] error, or [`LayoutError::DuplicatePeer`].
pub fn desk_layout(desk: &Desk) -> Result<Layout, LayoutError> {
    for (i, s) in desk.screens.iter().enumerate() {
        if desk.screens[..i].iter().any(|b| b.peer == s.peer) {
            return Err(LayoutError::DuplicatePeer(s.name.clone()));
        }
    }
    Layout::new(
        desk.screens
            .iter()
            .map(|d| Screen {
                name: d.name.clone(),
                rect: Rect {
                    x: d.x,
                    y: d.y,
                    w: d.w,
                    h: d.h,
                },
            })
            .collect(),
    )
}

/// More screens than any real desk; bounds the O(n²) overlap check on peer input.
pub const MAX_SCREENS: usize = 64;
/// Honest peers bump the version by one per edit, so a bigger jump is hostile or
/// broken; it would otherwise let one peer saturate the version and lock out edits.
pub(crate) const MAX_VERSION_JUMP: u64 = 1 << 20;

/// Why a received Desk document was refused by [`adopt`].
#[derive(Debug, PartialEq, Eq)]
pub enum DeskError {
    /// The screens do not form a valid [`Layout`].
    Layout(LayoutError),
    /// The document's `author` is not the authenticated peer that sent it.
    NotAuthor,
    /// The version moves forward by more than `MAX_VERSION_JUMP` (2^20).
    VersionJump,
    /// The document has more than [`MAX_SCREENS`] screens.
    TooManyScreens,
    /// A screen belongs to a peer this device has not paired with.
    UnknownPeer(PeerId),
}

/// A plain sentence for a person.
impl std::fmt::Display for DeskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layout(e) => write!(f, "{e}"),
            Self::NotAuthor => f.write_str("it was not written by the computer that sent it"),
            Self::VersionJump => f.write_str("its version jumps too far ahead"),
            Self::TooManyScreens => f.write_str("it has too many screens"),
            Self::UnknownPeer(_) => f.write_str("it places a computer that is not paired"),
        }
    }
}

impl std::error::Error for DeskError {}

/// Decides whether `incoming`, received from the authenticated peer `sender`, replaces
/// `current`: the higher `(version, author)` wins, and only if it is well-formed.
/// `known` says whether a peer is this device or one it has paired with.
/// `Ok(None)` means "keep `current`".
pub fn adopt(
    current: &Desk,
    incoming: Desk,
    sender: PeerId,
    known: impl Fn(&PeerId) -> bool,
) -> Result<Option<Desk>, DeskError> {
    if incoming.author != sender {
        return Err(DeskError::NotAuthor);
    }
    if (incoming.version, incoming.author) <= (current.version, current.author) {
        return Ok(None);
    }
    // Cannot underflow: passing the check above implies incoming.version >= current.version.
    if incoming.version - current.version > MAX_VERSION_JUMP {
        return Err(DeskError::VersionJump);
    }
    if incoming.screens.len() > MAX_SCREENS {
        return Err(DeskError::TooManyScreens);
    }
    if let Some(s) = incoming.screens.iter().find(|s| !known(&s.peer)) {
        return Err(DeskError::UnknownPeer(s.peer));
    }
    desk_layout(&incoming).map_err(DeskError::Layout)?;
    Ok(Some(incoming))
}

/// A local edit: the new document outranks `current` on every peer.
pub fn edit(current: &Desk, me: PeerId, screens: Vec<DeskScreen>) -> Desk {
    // Saturating: a peer-sent u64::MAX version must not make our next edit panic.
    Desk {
        version: current.version.saturating_add(1),
        author: me,
        screens,
    }
}

/// Maps a HID key usage typed on a `from` keyboard to the usage the `to` OS should see.
/// Mac <-> PC swaps Command and Control so Cmd+C on a Mac keyboard copies on Windows,
/// and Ctrl+C on a PC keyboard copies on the Mac.
pub(crate) fn remap_key(code: u32, from: Os, to: Os) -> u32 {
    if (from == Os::MacOs) == (to == Os::MacOs) {
        return code;
    }
    match code {
        0xE0 => 0xE3, // left Control -> left GUI (Command)
        0xE3 => 0xE0,
        0xE4 => 0xE7, // right Control -> right GUI
        0xE7 => 0xE4,
        _ => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle, for brevity; shared with the engine tests.
    pub(crate) fn r(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    fn screen(name: &str, x: i32, y: i32, w: u32, h: u32) -> Screen {
        Screen {
            name: name.into(),
            rect: r(x, y, w, h),
        }
    }

    /// A move that stayed on `screen`.
    fn within(screen: usize, x: i32, y: i32) -> Move {
        Move::Within { screen, x, y }
    }

    /// A move from screen `from` onto `to`, entering at (`x`, `y`).
    fn switched(from: usize, to: usize, x: i32, y: i32) -> Move {
        Move::Switched { from, to, x, y }
    }

    /// Laptop 1920x1080 on the left, 2560x1440 monitor on the right, tops aligned.
    fn two_screens() -> Layout {
        Layout::new(vec![
            screen("laptop", 0, 0, 1920, 1080),
            screen("desk", 1920, 0, 2560, 1440),
        ])
        .unwrap()
    }

    #[test]
    fn layout_rejects_invalid() {
        assert_eq!(Layout::new(vec![]).unwrap_err(), LayoutError::Empty);
        assert_eq!(
            Layout::new(vec![screen("a", 0, 0, 0, 10)]).unwrap_err(),
            LayoutError::ZeroSize("a".into())
        );
        assert_eq!(
            Layout::new(vec![screen("a", 0, 0, 10, 10), screen("a", 10, 0, 10, 10)]).unwrap_err(),
            LayoutError::DuplicateName("a".into())
        );
        assert_eq!(
            Layout::new(vec![screen("a", 0, 0, 10, 10), screen("b", 9, 9, 10, 10)]).unwrap_err(),
            LayoutError::Overlap("a".into(), "b".into())
        );
        assert_eq!(
            Layout::new(vec![screen("a", i32::MAX - 5, 0, 10, 10)]).unwrap_err(),
            LayoutError::OutOfRange("a".into())
        );
        // right edge fits i32, but width alone does not: local x would overflow.
        assert_eq!(
            Layout::new(vec![screen("a", -2_000_000_000, 0, 3_000_000_000, 10)]).unwrap_err(),
            LayoutError::OutOfRange("a".into())
        );
    }

    #[test]
    fn switcher_rejects_unknown_start_screen() {
        assert!(Switcher::new(two_screens(), 2).is_none());
    }

    #[test]
    fn touching_edges_do_not_overlap() {
        assert!(Layout::new(vec![screen("a", 0, 0, 10, 10), screen("b", 10, 0, 10, 10)]).is_ok());
    }

    #[test]
    fn starts_at_center_of_start_screen() {
        let mut s = Switcher::new(two_screens(), 1).unwrap();
        assert_eq!(s.move_by(0, 0), within(1, 1280, 720));
    }

    #[test]
    fn moving_inside_screen_stays() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        assert_eq!(s.move_by(10, -20), within(0, 970, 520));
    }

    #[test]
    fn crossing_right_edge_switches_with_local_entry_point() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        s.move_by(959, 0); // x = 1919, last laptop column
        assert_eq!(s.move_by(1, 0), switched(0, 1, 0, 720));
        assert_eq!(s.local().0, 1);
    }

    #[test]
    fn fast_flick_past_neighbour_lands_on_its_far_edge() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        assert_eq!(s.move_by(100_000, 0), switched(0, 1, 2559, 720));
    }

    #[test]
    fn edge_without_neighbour_clamps() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        assert_eq!(s.move_by(-5000, -5000), within(0, 0, 0));
    }

    #[test]
    fn single_neighbour_maps_the_edge_proportionally() {
        // Desk is 1440 tall, laptop 1080: the whole left edge of the desk leads onto the
        // laptop, scaled, instead of a dead zone below y = 1080.
        let mut s = Switcher::new(two_screens(), 1).unwrap();
        s.move_by(0, 600); // desk y = 1320
        assert_eq!(s.move_by(-5000, 0), switched(1, 0, 0, 990));
    }

    #[test]
    fn tall_mac_beside_short_pc_has_no_dead_zone() {
        let layout = Layout::new(vec![screen("mac", 0, 0, 3840, 2160), screen("pc", 3840, 0, 1920, 1080)]).unwrap();
        let mut s = Switcher::new(layout, 0).unwrap();
        s.set_local(0, 3839, 1959);
        assert_eq!(s.move_by(5, 0), switched(0, 1, 4, 979));
        assert_eq!(s.move_by(-6, 0), switched(1, 0, 3838, 1958));
    }

    #[test]
    fn several_neighbours_on_one_edge_keep_absolute_positions() {
        // Two monitors stacked to the right: each keeps its own band.
        let layout = Layout::new(vec![
            screen("a", 0, 0, 100, 200),
            screen("top", 100, 0, 100, 100),
            screen("bottom", 100, 100, 100, 100),
        ])
        .unwrap();
        let mut s = Switcher::new(layout, 0).unwrap();
        s.set_local(0, 99, 150);
        assert_eq!(s.move_by(1, 0), switched(0, 2, 0, 50));
    }

    #[test]
    fn round_trip_returns_to_original_screen() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        s.move_by(960, 0);
        assert_eq!(s.move_by(-1, 0), switched(1, 0, 1919, 540));
    }

    #[test]
    fn diagonal_corner_exit_still_switches() {
        // Screen b sits to the right of a; leaving a's top-right corner diagonally
        // up-right has nothing above, but b is to the right.
        let layout = Layout::new(vec![screen("a", 0, 0, 100, 100), screen("b", 100, 0, 100, 100)]).unwrap();
        let mut s = Switcher::new(layout, 0).unwrap();
        s.move_by(49, -50); // (99, 0) top-right pixel
        assert_eq!(s.move_by(5, -5), switched(0, 1, 4, 0));
    }

    const L_CTRL: u32 = 0xE0;
    const L_ALT: u32 = 0xE2;
    const L_GUI: u32 = 0xE3;
    const R_CTRL: u32 = 0xE4;
    const R_GUI: u32 = 0xE7;
    const KEY_C: u32 = 0x06;

    #[test]
    fn mac_to_pc_swaps_command_and_control() {
        for to in [Os::Windows, Os::Linux] {
            assert_eq!(remap_key(L_GUI, Os::MacOs, to), L_CTRL);
            assert_eq!(remap_key(L_CTRL, Os::MacOs, to), L_GUI);
            assert_eq!(remap_key(R_GUI, Os::MacOs, to), R_CTRL);
            assert_eq!(remap_key(R_CTRL, Os::MacOs, to), R_GUI);
        }
    }

    #[test]
    fn pc_to_mac_swaps_control_and_command() {
        assert_eq!(remap_key(L_CTRL, Os::Windows, Os::MacOs), L_GUI);
        assert_eq!(remap_key(L_GUI, Os::Windows, Os::MacOs), L_CTRL);
    }

    #[test]
    fn same_family_and_other_keys_pass_through() {
        assert_eq!(remap_key(L_GUI, Os::MacOs, Os::MacOs), L_GUI);
        assert_eq!(remap_key(L_CTRL, Os::Windows, Os::Linux), L_CTRL);
        assert_eq!(remap_key(L_ALT, Os::MacOs, Os::Windows), L_ALT);
        assert_eq!(remap_key(KEY_C, Os::MacOs, Os::Windows), KEY_C);
    }

    fn desk(version: u64, author: u8, screens: Vec<DeskScreen>) -> Desk {
        Desk {
            version,
            author: PeerId([author; 16]),
            screens,
        }
    }

    /// Adopts as if the document's own author sent it and every peer is known.
    fn take(cur: &Desk, incoming: Desk) -> Result<Option<Desk>, DeskError> {
        let sender = incoming.author;
        adopt(cur, incoming, sender, |_| true)
    }

    /// A 100x100 screen owned by a peer derived from the screen's name, so names are distinct peers.
    fn ds(name: &str, x: i32) -> DeskScreen {
        DeskScreen {
            peer: PeerId([name.as_bytes()[0]; 16]),
            name: name.into(),
            x,
            y: 0,
            w: 100,
            h: 100,
        }
    }

    #[test]
    fn newer_desk_wins_older_is_ignored() {
        let cur = desk(5, 1, vec![ds("a", 0)]);
        let newer = desk(6, 0, vec![ds("a", 0), ds("b", 100)]);
        assert_eq!(take(&cur, newer.clone()), Ok(Some(newer)));
        assert_eq!(take(&cur, desk(4, 9, vec![ds("a", 0)])), Ok(None));
        assert_eq!(take(&cur, cur.clone()), Ok(None), "same document");
    }

    #[test]
    fn two_screens_for_one_peer_are_refused() {
        let mut twin = ds("b", 100);
        twin.peer = ds("a", 0).peer;
        let d = desk(1, 1, vec![ds("a", 0), twin]);
        assert_eq!(desk_layout(&d).unwrap_err(), LayoutError::DuplicatePeer("b".into()));
        assert_eq!(
            take(&desk(0, 1, vec![]), d),
            Err(DeskError::Layout(LayoutError::DuplicatePeer("b".into())))
        );
    }

    #[test]
    fn snapping_survives_extreme_coordinates() {
        let m = [r(0, 0, 10, 10), r(20, 0, 10, 10)];
        assert_eq!(snap_to_monitors(&m, i32::MIN, i32::MIN), (0, 0));
        assert_eq!(snap_to_monitors(&m, i32::MAX, i32::MIN), (29, 0));
        assert_eq!(snap_to_monitors(&m, i32::MIN, i32::MAX), (0, 9));
        assert_eq!(snap_to_monitors(&m, i32::MAX, i32::MAX), (29, 9));
    }

    #[test]
    fn exit_push_with_no_monitors_uses_the_whole_box() {
        assert_eq!(exit_push(&[], (100, 100), (99, 50), (5, 0)), (5, 0));
        assert_eq!(
            exit_push(&[], (100, 100), (50, 50), (5, 0)),
            (0, 0),
            "inside the box: stays"
        );
        assert_eq!(exit_push(&[], (100, 100), (0, 50), (-5, 0)), (-5, 0));
    }

    #[test]
    fn exit_push_carry_saturates_instead_of_wrapping() {
        // A small monitor in a box wider than i32::MAX: the carry must not wrap negative.
        let m = [r(0, 0, 10, 10)];
        assert_eq!(exit_push(&m, (u32::MAX, 10), (9, 5), (1, 0)).0, i32::MAX);
    }

    #[test]
    fn equal_versions_break_ties_by_author() {
        let cur = desk(5, 1, vec![ds("a", 0)]);
        let higher = desk(5, 2, vec![ds("b", 0)]);
        assert_eq!(take(&cur, higher.clone()), Ok(Some(higher)));
        assert_eq!(take(&cur, desk(5, 0, vec![ds("c", 0)])), Ok(None));
    }

    #[test]
    fn newer_but_invalid_desk_is_rejected() {
        let cur = desk(1, 1, vec![ds("a", 0)]);
        let overlapping = desk(9, 1, vec![ds("a", 0), ds("b", 50)]);
        assert_eq!(
            take(&cur, overlapping),
            Err(DeskError::Layout(LayoutError::Overlap("a".into(), "b".into())))
        );
    }

    #[test]
    fn local_edit_outranks_current_everywhere() {
        let cur = desk(7, 200, vec![ds("a", 0)]);
        let mine = edit(&cur, PeerId([1; 16]), vec![ds("a", 0), ds("b", 100)]);
        assert_eq!((mine.version, mine.author), (8, PeerId([1; 16])));
        assert_eq!(take(&cur, mine.clone()), Ok(Some(mine)));
        let maxed = desk(u64::MAX, 0, vec![ds("a", 0)]);
        assert_eq!(edit(&maxed, PeerId([1; 16]), vec![]).version, u64::MAX);
    }

    #[test]
    fn desk_must_come_from_its_author() {
        let cur = desk(1, 1, vec![ds("a", 0)]);
        let forged = desk(2, 9, vec![ds("a", 0)]);
        assert_eq!(
            adopt(&cur, forged, PeerId([3; 16]), |_| true),
            Err(DeskError::NotAuthor)
        );
    }

    #[test]
    fn huge_version_jump_is_rejected() {
        let cur = desk(10, 1, vec![ds("a", 0)]);
        assert_eq!(
            take(&cur, desk(u64::MAX, 2, vec![ds("a", 0)])),
            Err(DeskError::VersionJump)
        );
        assert!(
            take(&cur, desk(10 + MAX_VERSION_JUMP, 2, vec![ds("a", 0)]))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn too_many_screens_is_rejected_before_layout_check() {
        let cur = desk(1, 1, vec![ds("a", 0)]);
        let screens = (0..=MAX_SCREENS as i32).map(|i| ds(&i.to_string(), i * 100)).collect();
        assert_eq!(take(&cur, desk(2, 1, screens)), Err(DeskError::TooManyScreens));
    }

    #[test]
    fn screens_of_unknown_peers_are_rejected() {
        let cur = desk(1, 1, vec![ds("a", 0)]);
        let mut stranger = ds("b", 100);
        stranger.peer = PeerId([0xEE; 16]);
        let incoming = desk(2, 1, vec![ds("a", 0), stranger]);
        let known = |p: &PeerId| *p != PeerId([0xEE; 16]);
        assert_eq!(
            adopt(&cur, incoming, PeerId([1; 16]), known),
            Err(DeskError::UnknownPeer(PeerId([0xEE; 16])))
        );
    }

    /// Laptop 1440x900 low-left beside a 1920x1080 display: box 3360x1080, hole top-left.
    fn l_top_hole() -> [Rect; 2] {
        [r(0, 180, 1440, 900), r(1440, 0, 1920, 1080)]
    }

    /// The same pair top-aligned: hole bottom-left.
    fn l_bottom_hole() -> [Rect; 2] {
        [r(0, 0, 1440, 900), r(1440, 0, 1920, 1080)]
    }

    #[test]
    fn snapping_moves_a_point_in_a_hole_onto_the_nearest_monitor() {
        let m = l_top_hole();
        assert_eq!(snap_to_monitors(&m, 100, 500), (100, 500), "on a monitor: unchanged");
        assert_eq!(snap_to_monitors(&m, 100, 10), (100, 180), "hole above the laptop");
        assert_eq!(snap_to_monitors(&m, 0, 0), (0, 180));
        assert_eq!(snap_to_monitors(&[], 7, 9), (7, 9), "no monitors: unchanged");
        let m = l_bottom_hole();
        assert_eq!(snap_to_monitors(&m, 1000, 1000), (1000, 899), "hole below the laptop");
    }

    #[test]
    fn a_single_monitor_is_the_plain_box_rule() {
        let b = [r(0, 0, 100, 50)];
        let push = |p, d| exit_push(&b, (100, 50), p, d);
        assert_eq!(push((99, 20), (5, 0)), (5, 0), "at the right edge pushing out");
        assert_eq!(push((98, 20), (5, 0)), (0, 0), "inside the screen");
        assert_eq!(push((50, 0), (0, -3)), (0, -3));
        assert_eq!(push((0, 20), (3, 0)), (0, 0), "at the edge but pushing in");
        assert_eq!(push((0, 20), (-3, 0)), (-3, 0));
    }

    #[test]
    fn l_shape_leaves_over_a_hole_facing_edge_towards_the_box_edge() {
        let m = l_top_hole();
        let push = |p, d| exit_push(&m, (3360, 1080), p, d);
        // Laptop top edge faces the hole: pushing up leaves through the box top.
        assert_eq!(push((500, 180), (0, -4)), (0, -4 - 180));
        // Not at the edge: stays.
        assert_eq!(push((500, 300), (0, -4)), (0, 0));
        // Laptop's left edge is the box edge.
        assert_eq!(push((0, 500), (-4, 0)), (-4, 0));
        // Laptop right edge borders the display: no exit.
        assert_eq!(push((1439, 500), (4, 0)), (0, 0));
        // Display top is the box top; its right edge the box right edge.
        assert_eq!(push((2000, 0), (0, -4)), (0, -4));
        assert_eq!(push((3359, 500), (4, 0)), (4, 0));
        // Laptop bottom (1079) is the box bottom.
        assert_eq!(push((500, 1079), (0, 4)), (0, 4));
    }

    #[test]
    fn l_shape_with_the_hole_below_leaves_over_the_laptop_bottom() {
        let m = l_bottom_hole();
        let push = |p, d| exit_push(&m, (3360, 1080), p, d);
        // Laptop bottom (899) faces the hole: push down carries to the box bottom.
        assert_eq!(push((500, 899), (0, 4)), (0, 4 + 180));
        assert_eq!(push((1439, 500), (4, 0)), (0, 0), "inner edge to the display");
        assert_eq!(push((0, 500), (-4, 0)), (-4, 0));
    }

    #[test]
    fn a_monitor_stacked_above_has_only_the_outer_edges() {
        let m = [r(0, 0, 1000, 600), r(0, 600, 1000, 600)];
        let push = |p, d| exit_push(&m, (1000, 1200), p, d);
        assert_eq!(push((10, 599), (0, 5)), (0, 0), "inner edge between the two");
        assert_eq!(push((10, 600), (0, -5)), (0, 0));
        assert_eq!(push((10, 0), (0, -5)), (0, -5));
        assert_eq!(push((10, 1199), (0, 5)), (0, 5));
    }

    #[test]
    fn a_monitor_ahead_in_line_blocks_leaving_even_across_a_gap() {
        // A gap between two monitors on the same row: the far one is in the way.
        let m = [r(0, 0, 100, 100), r(200, 0, 100, 100)];
        assert_eq!(exit_push(&m, (300, 100), (99, 50), (5, 0)), (0, 0));
    }
}
