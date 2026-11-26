// SPDX-License-Identifier: AGPL-3.0-only
//! Platform-free Overdesk logic: screen layout, cursor handoff, held-key tracking.

use std::collections::BTreeSet;

pub mod engine;

pub use overdesk_proto::{Desk, DeskScreen, Os, PeerId};

/// Screen rectangle in the shared logical (DPI-independent) desktop plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    // i64 so x + w cannot overflow; Layout::new guarantees results fit i32.
    fn right(&self) -> i64 {
        self.x as i64 + self.w as i64
    }

    fn bottom(&self) -> i64 {
        self.y as i64 + self.h as i64
    }

    fn contains(&self, (x, y): (i64, i64)) -> bool {
        (self.x as i64..self.right()).contains(&x) && (self.y as i64..self.bottom()).contains(&y)
    }

    fn clamp(&self, (x, y): (i64, i64)) -> (i64, i64) {
        (
            x.clamp(self.x as i64, self.right() - 1),
            y.clamp(self.y as i64, self.bottom() - 1),
        )
    }

    fn overlaps(&self, o: &Rect) -> bool {
        (self.x as i64) < o.right()
            && (o.x as i64) < self.right()
            && (self.y as i64) < o.bottom()
            && (o.y as i64) < self.bottom()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub name: String,
    pub rect: Rect,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    Empty,
    ZeroSize(String),
    OutOfRange(String),
    DuplicateName(String),
    Overlap(String, String),
}

#[derive(Debug, Clone)]
pub struct Layout {
    screens: Vec<Screen>,
}

impl Layout {
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

    fn find(&self, p: (i64, i64)) -> Option<usize> {
        self.screens.iter().position(|s| s.rect.contains(p))
    }

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

/// Result of a relative cursor move. Coordinates are local to `screen` / `to`.
#[derive(Debug, PartialEq, Eq)]
pub enum Motion {
    Within { screen: usize, x: i32, y: i32 },
    Switched { from: usize, to: usize, x: i32, y: i32 },
}

/// Tracks which screen owns the cursor, driven by relative mouse deltas.
#[derive(Debug)]
pub struct Switcher {
    layout: Layout,
    active: usize,
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

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Re-syncs to where the OS says the cursor is (`x`, `y` local to `screen`), clamped.
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

    pub fn move_by(&mut self, dx: i32, dy: i32) -> Motion {
        let from = self.active;
        let target = (self.pos.0 as i64 + dx as i64, self.pos.1 as i64 + dy as i64);
        let cur = self.layout.screens[from].rect;
        if cur.contains(target) {
            return self.settle(target, |screen, x, y| Motion::Within { screen, x, y });
        }
        let edge = cur.clamp(target);
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
                return self.settle(landing, |to, x, y| Motion::Switched { from, to, x, y });
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
                return self.settle(landing, |to, x, y| Motion::Switched { from, to, x, y });
            }
        }
        self.settle(edge, |screen, x, y| Motion::Within { screen, x, y })
    }

    /// Stores `p` (already inside the active screen) and reports it in local coordinates.
    fn settle(&mut self, p: (i64, i64), make: impl FnOnce(usize, i32, i32) -> Motion) -> Motion {
        self.pos = (p.0 as i32, p.1 as i32);
        let r = self.layout.screens[self.active].rect;
        make(self.active, self.pos.0 - r.x, self.pos.1 - r.y)
    }
}

/// Remembers held keys/buttons so they can be released when focus leaves a screen
/// or a peer disconnects (otherwise the remote sees a stuck key).
#[derive(Debug, Default)]
pub struct HeldKeys(BTreeSet<u32>);

impl HeldKeys {
    pub fn press(&mut self, code: u32) {
        self.0.insert(code);
    }

    pub fn release(&mut self, code: u32) {
        self.0.remove(&code);
    }

    /// Returns every held code (ascending) and forgets them.
    pub fn release_all(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.0).into_iter().collect()
    }
}

/// Builds the validated layout a Desk document describes.
pub fn desk_layout(desk: &Desk) -> Result<Layout, LayoutError> {
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
pub const MAX_VERSION_JUMP: u64 = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum DeskError {
    Layout(LayoutError),
    /// The document's `author` is not the authenticated peer that sent it.
    NotAuthor,
    VersionJump,
    TooManyScreens,
    /// A screen belongs to a peer this device has not paired with.
    UnknownPeer(PeerId),
}

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
pub fn remap_key(code: u32, from: Os, to: Os) -> u32 {
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

    fn screen(name: &str, x: i32, y: i32, w: u32, h: u32) -> Screen {
        Screen {
            name: name.into(),
            rect: Rect { x, y, w, h },
        }
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
        assert_eq!(
            s.move_by(0, 0),
            Motion::Within {
                screen: 1,
                x: 1280,
                y: 720
            }
        );
    }

    #[test]
    fn moving_inside_screen_stays() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        assert_eq!(
            s.move_by(10, -20),
            Motion::Within {
                screen: 0,
                x: 970,
                y: 520
            }
        );
    }

    #[test]
    fn crossing_right_edge_switches_with_local_entry_point() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        s.move_by(959, 0); // x = 1919, last laptop column
        assert_eq!(
            s.move_by(1, 0),
            Motion::Switched {
                from: 0,
                to: 1,
                x: 0,
                y: 720
            }
        );
        assert_eq!(s.active(), 1);
    }

    #[test]
    fn fast_flick_past_neighbour_lands_on_its_far_edge() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        assert_eq!(
            s.move_by(100_000, 0),
            Motion::Switched {
                from: 0,
                to: 1,
                x: 2559,
                y: 720
            }
        );
    }

    #[test]
    fn edge_without_neighbour_clamps() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        assert_eq!(s.move_by(-5000, -5000), Motion::Within { screen: 0, x: 0, y: 0 });
    }

    #[test]
    fn single_neighbour_maps_the_edge_proportionally() {
        // Desk is 1440 tall, laptop 1080: the whole left edge of the desk leads onto the
        // laptop, scaled, instead of a dead zone below y = 1080.
        let mut s = Switcher::new(two_screens(), 1).unwrap();
        s.move_by(0, 600); // desk y = 1320
        assert_eq!(
            s.move_by(-5000, 0),
            Motion::Switched {
                from: 1,
                to: 0,
                x: 0,
                y: 990
            }
        );
    }

    #[test]
    fn tall_mac_beside_short_pc_has_no_dead_zone() {
        let layout = Layout::new(vec![screen("mac", 0, 0, 3840, 2160), screen("pc", 3840, 0, 1920, 1080)]).unwrap();
        let mut s = Switcher::new(layout, 0).unwrap();
        s.set_local(0, 3839, 1959);
        assert_eq!(
            s.move_by(5, 0),
            Motion::Switched {
                from: 0,
                to: 1,
                x: 4,
                y: 979
            }
        );
        assert_eq!(
            s.move_by(-6, 0),
            Motion::Switched {
                from: 1,
                to: 0,
                x: 3838,
                y: 1958
            }
        );
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
        assert_eq!(
            s.move_by(1, 0),
            Motion::Switched {
                from: 0,
                to: 2,
                x: 0,
                y: 50
            }
        );
    }

    #[test]
    fn round_trip_returns_to_original_screen() {
        let mut s = Switcher::new(two_screens(), 0).unwrap();
        s.move_by(960, 0);
        assert_eq!(
            s.move_by(-1, 0),
            Motion::Switched {
                from: 1,
                to: 0,
                x: 1919,
                y: 540
            }
        );
    }

    #[test]
    fn diagonal_corner_exit_still_switches() {
        // Screen b sits to the right of a; leaving a's top-right corner diagonally
        // up-right has nothing above, but b is to the right.
        let layout = Layout::new(vec![screen("a", 0, 0, 100, 100), screen("b", 100, 0, 100, 100)]).unwrap();
        let mut s = Switcher::new(layout, 0).unwrap();
        s.move_by(49, -50); // (99, 0) top-right pixel
        assert_eq!(
            s.move_by(5, -5),
            Motion::Switched {
                from: 0,
                to: 1,
                x: 4,
                y: 0
            }
        );
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

    #[test]
    fn held_keys_released_once_in_order() {
        let mut k = HeldKeys::default();
        k.press(0xE1);
        k.press(0x04);
        k.press(0x04);
        k.press(0x05);
        k.release(0x05);
        assert_eq!(k.release_all(), vec![0x04, 0xE1]);
        assert_eq!(k.release_all(), Vec::<u32>::new());
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

    fn ds(name: &str, x: i32) -> DeskScreen {
        DeskScreen {
            peer: PeerId([0; 16]),
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
}
