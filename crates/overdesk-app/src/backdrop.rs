// SPDX-License-Identifier: AGPL-3.0-only
//! The full-window backdrop: the brand's near-black or near-white, lit by a few large,
//! soft glows. iced's canvas has linear gradients only, so each glow is a stack of
//! concentric circles whose combined opacity falls off smoothly toward the edge.

use iced::mouse::Cursor;
use iced::widget::canvas::{Frame, Geometry, Path, Program};
use iced::{Color, Point, Rectangle, Renderer, Theme};

use crate::style::{INDIGO, INK, PAPER, TEAL, VIOLET, alpha, is_dark};

/// How many circles make up one glow; more steps mean a smoother falloff.
const STEPS: u32 = 48;

/// The backdrop widget; it has no state of its own.
pub struct Backdrop;

/// Draws one radial glow: `peak` opacity at `center`, fading to nothing at `radius`.
fn glow(frame: &mut Frame, center: Point, radius: f32, color: Color, peak: f32) {
    // Per-circle opacity chosen so the stack composites to `peak` at the centre.
    let a = 1.0 - (1.0 - peak).powf(1.0 / STEPS as f32);
    for i in 0..STEPS {
        // Squaring the step eases the falloff, so the edge dissolves instead of ending.
        let t = 1.0 - i as f32 / STEPS as f32;
        frame.fill(&Path::circle(center, radius * t.sqrt()), alpha(color, a));
    }
}

impl<Message> Program<Message> for Backdrop {
    type State = ();

    fn draw(&self, _: &(), renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let dark = is_dark(theme);
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), if dark { INK } else { PAPER });
        let (w, h) = (bounds.width, bounds.height);
        let big = w.max(h);
        // Dark mode carries about 18-28% glows; light mode about 10-14%.
        let (violet, indigo, teal) = if dark { (0.26, 0.22, 0.1) } else { (0.13, 0.11, 0.06) };
        glow(&mut frame, Point::new(w * 0.08, h * 0.02), big * 0.8, VIOLET, violet);
        glow(&mut frame, Point::new(w * 0.98, h * 0.95), big * 0.75, INDIGO, indigo);
        glow(&mut frame, Point::new(w * 0.88, h * 0.06), big * 0.4, TEAL, teal);
        vec![frame.into_geometry()]
    }
}
