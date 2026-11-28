// SPDX-License-Identifier: AGPL-3.0-only
//! The one rectangle type: monitors, desktop boxes and Desk screens.

/// A screen or monitor rectangle in a global desktop plane: the Desk's logical (DPI-independent)
/// coordinates in the engine, OS pixels or points in the platform adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge in global logical coordinates.
    pub x: i32,
    /// Top edge in global logical coordinates.
    pub y: i32,
    /// Width in logical pixels.
    pub w: u32,
    /// Height in logical pixels.
    pub h: u32,
}

impl Rect {
    /// The `w` x `h` rectangle at the origin: a screen's whole box in its own coordinates.
    pub fn whole(w: u32, h: u32) -> Self {
        Self { x: 0, y: 0, w, h }
    }

    /// Exclusive right edge (`x + w`) in global logical coordinates.
    // i64 so x + w cannot overflow; Layout::new guarantees results fit i32.
    pub fn right(&self) -> i64 {
        self.x as i64 + self.w as i64
    }

    /// Exclusive bottom edge (`y + h`) in global logical coordinates.
    pub fn bottom(&self) -> i64 {
        self.y as i64 + self.h as i64
    }

    /// Whether the global point lies inside; right and bottom edges are exclusive.
    pub fn contains(&self, (x, y): (i64, i64)) -> bool {
        (self.x as i64..self.right()).contains(&x) && (self.y as i64..self.bottom()).contains(&y)
    }

    /// Nearest pixel inside the rectangle to the global point. Requires a non-zero size.
    pub fn clamp(&self, (x, y): (i64, i64)) -> (i64, i64) {
        (
            x.clamp(self.x as i64, self.right() - 1),
            y.clamp(self.y as i64, self.bottom() - 1),
        )
    }

    /// Whether the two rectangles share any pixel; rectangles that only touch do not.
    pub fn overlaps(&self, o: &Rect) -> bool {
        (self.x as i64) < o.right()
            && (o.x as i64) < self.right()
            && (self.y as i64) < o.bottom()
            && (o.y as i64) < self.bottom()
    }
}
