// SPDX-License-Identifier: AGPL-3.0-only
//! Platform-free Overdesk logic: screen layout, cursor handoff, held-key tracking.

use std::collections::BTreeSet;

/// Screen rectangle in the shared logical (DPI-independent) desktop plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
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
    DuplicateName(String),
    Overlap(String, String),
}

#[derive(Debug, Clone)]
pub struct Layout {
    screens: Vec<Screen>,
}

impl Layout {
    pub fn new(_screens: Vec<Screen>) -> Result<Self, LayoutError> {
        todo!()
    }

    pub fn screens(&self) -> &[Screen] {
        &self.screens
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
    /// Starts at the center of screen `start`.
    pub fn new(_layout: Layout, _start: usize) -> Self {
        todo!()
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn move_by(&mut self, _dx: i32, _dy: i32) -> Motion {
        todo!()
    }
}

/// Remembers held keys/buttons so they can be released when focus leaves a screen
/// or a peer disconnects (otherwise the remote sees a stuck key).
#[derive(Debug, Default)]
pub struct HeldKeys(BTreeSet<u32>);

impl HeldKeys {
    pub fn press(&mut self, _code: u32) {
        todo!()
    }

    pub fn release(&mut self, _code: u32) {
        todo!()
    }

    /// Returns every held code (ascending) and forgets them.
    pub fn release_all(&mut self) -> Vec<u32> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(name: &str, x: i32, y: i32, w: u32, h: u32) -> Screen {
        Screen { name: name.into(), rect: Rect { x, y, w, h } }
    }

    /// Laptop 1920x1080 on the left, 2560x1440 monitor on the right, tops aligned.
    fn two_screens() -> Layout {
        Layout::new(vec![screen("laptop", 0, 0, 1920, 1080), screen("desk", 1920, 0, 2560, 1440)]).unwrap()
    }

    #[test]
    fn layout_rejects_invalid() {
        assert_eq!(Layout::new(vec![]).unwrap_err(), LayoutError::Empty);
        assert_eq!(Layout::new(vec![screen("a", 0, 0, 0, 10)]).unwrap_err(), LayoutError::ZeroSize("a".into()));
        assert_eq!(
            Layout::new(vec![screen("a", 0, 0, 10, 10), screen("a", 10, 0, 10, 10)]).unwrap_err(),
            LayoutError::DuplicateName("a".into())
        );
        assert_eq!(
            Layout::new(vec![screen("a", 0, 0, 10, 10), screen("b", 9, 9, 10, 10)]).unwrap_err(),
            LayoutError::Overlap("a".into(), "b".into())
        );
    }

    #[test]
    fn touching_edges_do_not_overlap() {
        assert!(Layout::new(vec![screen("a", 0, 0, 10, 10), screen("b", 10, 0, 10, 10)]).is_ok());
    }

    #[test]
    fn starts_at_center_of_start_screen() {
        let mut s = Switcher::new(two_screens(), 1);
        assert_eq!(s.move_by(0, 0), Motion::Within { screen: 1, x: 1280, y: 720 });
    }

    #[test]
    fn moving_inside_screen_stays() {
        let mut s = Switcher::new(two_screens(), 0);
        assert_eq!(s.move_by(10, -20), Motion::Within { screen: 0, x: 970, y: 520 });
    }

    #[test]
    fn crossing_right_edge_switches_with_local_entry_point() {
        let mut s = Switcher::new(two_screens(), 0);
        s.move_by(959, 0); // x = 1919, last laptop column
        assert_eq!(s.move_by(1, 0), Motion::Switched { from: 0, to: 1, x: 0, y: 540 });
        assert_eq!(s.active(), 1);
    }

    #[test]
    fn fast_flick_past_neighbour_lands_on_its_far_edge() {
        let mut s = Switcher::new(two_screens(), 0);
        assert_eq!(s.move_by(100_000, 0), Motion::Switched { from: 0, to: 1, x: 2559, y: 540 });
    }

    #[test]
    fn edge_without_neighbour_clamps() {
        let mut s = Switcher::new(two_screens(), 0);
        assert_eq!(s.move_by(-5000, -5000), Motion::Within { screen: 0, x: 0, y: 0 });
    }

    #[test]
    fn taller_neighbour_region_without_counterpart_clamps() {
        // Desk is taller: below y=1080 there is nothing to the left.
        let mut s = Switcher::new(two_screens(), 1);
        s.move_by(0, 600); // y = 1320
        assert_eq!(s.move_by(-5000, 0), Motion::Within { screen: 1, x: 0, y: 1320 });
    }

    #[test]
    fn round_trip_returns_to_original_screen() {
        let mut s = Switcher::new(two_screens(), 0);
        s.move_by(960, 0);
        assert_eq!(s.move_by(-1, 0), Motion::Switched { from: 1, to: 0, x: 1919, y: 540 });
    }

    #[test]
    fn diagonal_corner_exit_still_switches() {
        // Screen b sits to the right of a; leaving a's top-right corner diagonally
        // up-right has nothing above, but b is to the right.
        let layout = Layout::new(vec![screen("a", 0, 0, 100, 100), screen("b", 100, 0, 100, 100)]).unwrap();
        let mut s = Switcher::new(layout, 0);
        s.move_by(49, -50); // (99, 0) top-right pixel
        assert_eq!(s.move_by(5, -5), Motion::Switched { from: 0, to: 1, x: 4, y: 0 });
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
}
