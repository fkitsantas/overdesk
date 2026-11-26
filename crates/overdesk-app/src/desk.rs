// SPDX-License-Identifier: AGPL-3.0-only
//! The Desk map: real screens to scale, joined by the brand ribbon. The screen with the
//! cursor glows teal; while crossing, light flows along the ribbon toward it.

use iced::mouse::Cursor;
use iced::widget::canvas::{self, Frame, Geometry, Path, Program, Stroke, Text, gradient::Linear};
use iced::{Point, Rectangle, Renderer, Size, Theme, alignment};
use overdesk_core::engine::Mode;
use overdesk_proto::PeerId;
use overdeskd::controller::ScreenView;

use crate::style::{INDIGO, INK, TEAL, VIOLET, alpha, hairline, ink, is_dark, muted};

/// The Desk map widget: draws the shared Desk's screens, scaled to fit, on a canvas.
pub struct DeskMap<'a> {
    /// The screens to draw, in Desk coordinates.
    pub screens: &'a [ScreenView],
    /// The device whose screen has the cursor, which decides the teal screen.
    pub focus: Option<PeerId>,
    /// Where the cursor is, which decides the ribbon's flow.
    pub mode: Mode,
    /// 0.0..1.0, advanced by animation frames while the cursor is away.
    pub phase: f32,
}

/// The space between neighbouring screens, in pixels.
const GAP: f32 = 24.0;
/// The margin around the drawn Desk, in pixels.
const PAD: f32 = 20.0;

impl<Message> Program<Message> for DeskMap<'_> {
    type State = ();

    fn draw(&self, _: &(), renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        if self.screens.is_empty() {
            return vec![frame.into_geometry()];
        }
        let Some(rects) = layout(self.screens, bounds.width, bounds.height) else {
            // A degenerate Desk has nothing to scale; draw nothing rather than divide by zero.
            return vec![frame.into_geometry()];
        };
        let rects: Vec<(Rectangle, &ScreenView)> = rects.into_iter().zip(self.screens).collect();

        for (r, s) in &rects {
            let on = self.focus == Some(s.id);
            let face = Path::rounded_rectangle(r.position(), r.size(), 12.0.into());
            // Depth: a soft shadow below, then a slightly larger bezel behind the face.
            for i in 0..4 {
                let grow = i as f32 * 3.0;
                let soft = Path::rounded_rectangle(
                    Point::new(r.x - grow, r.y + 8.0 - grow),
                    Size::new(r.width + 2.0 * grow, r.height + 2.0 * grow),
                    (12.0 + grow).into(),
                );
                frame.fill(&soft, alpha(INK, if is_dark(theme) { 0.07 } else { 0.025 }));
            }
            let bezel = Path::rounded_rectangle(
                Point::new(r.x - 4.0, r.y - 4.0),
                Size::new(r.width + 8.0, r.height + 8.0),
                16.0.into(),
            );
            frame.fill(&bezel, alpha(ink(theme), if is_dark(theme) { 0.1 } else { 0.05 }));
            frame.stroke(&bezel, Stroke::default().with_color(hairline(theme)).with_width(1.0));
            if on {
                // The glow: strokes that grow outward while fading, around the teal outline.
                for i in 1..=6 {
                    let grow = 4.0 + i as f32 * 2.5;
                    let halo = Path::rounded_rectangle(
                        Point::new(r.x - grow, r.y - grow),
                        Size::new(r.width + 2.0 * grow, r.height + 2.0 * grow),
                        (12.0 + grow).into(),
                    );
                    frame.stroke(
                        &halo,
                        Stroke::default()
                            .with_color(alpha(TEAL, 0.2 / i as f32))
                            .with_width(2.5),
                    );
                }
            }
            frame.fill(&face, if on { alpha(TEAL, 0.14) } else { alpha(ink(theme), 0.04) });
            frame.stroke(
                &if on { bezel.clone() } else { face.clone() },
                Stroke::default()
                    .with_color(if on { TEAL } else { hairline(theme) })
                    .with_width(if on { 2.0 } else { 1.0 }),
            );
            frame.fill_text(Text {
                content: s.name.clone(),
                position: Point::new(r.x + 14.0, r.y + 14.0),
                color: ink(theme),
                size: 14.0.into(),
                font: iced::Font {
                    weight: iced::font::Weight::Semibold,
                    ..iced::Font::with_name("Inter")
                },
                align_y: alignment::Vertical::Top,
                ..Text::default()
            });
            frame.fill_text(Text {
                content: if s.me {
                    "This device".into()
                } else {
                    format!("{} × {}", s.w, s.h)
                },
                position: Point::new(r.x + 14.0, r.y + 34.0),
                color: muted(theme),
                size: 12.0.into(),
                align_y: alignment::Vertical::Top,
                ..Text::default()
            });
        }

        // A ribbon across every edge two screens share.
        for (i, (a, sa)) in rects.iter().enumerate() {
            for (b, sb) in &rects[i + 1..] {
                if !touch(sa, sb) {
                    continue;
                }
                // Ordered left to right, or top to bottom, so `b` is the right-hand or lower screen.
                let horizontal = sa.x + sa.w as i32 == sb.x || sb.x + sb.w as i32 == sa.x;
                let ((a, sa), (b, sb)) = if (horizontal && sb.x < sa.x) || (!horizontal && sb.y < sa.y) {
                    ((b, sb), (a, sa))
                } else {
                    ((a, sa), (b, sb))
                };
                // Starts low in the first screen and ends low in the second, clear of the labels.
                // A stacked pair keeps to the left and right, away from any side-by-side ribbon.
                let (sx, ex) = if horizontal { (0.5, 0.5) } else { (0.2, 0.8) };
                // Never above the name and size labels (they end 46 px down), never past the bottom edge.
                let low = |r: &Rectangle, frac: f32| (r.y + r.height * frac).max(r.y + 56.0).min(r.y + r.height - 8.0);
                let start = Point::new(a.x + a.width * sx, low(a, 0.82));
                let end = Point::new(b.x + b.width * ex, low(b, 0.72));
                let ribbon = Path::new(|p| {
                    p.move_to(start);
                    if horizontal {
                        let mid = (a.x + a.width + b.x) / 2.0;
                        p.bezier_curve_to(Point::new(mid, start.y), Point::new(mid, end.y), end);
                    } else {
                        let mid = (a.y + a.height + b.y) / 2.0;
                        p.bezier_curve_to(Point::new(start.x, mid), Point::new(end.x, mid), end);
                    }
                });
                let gradient = Linear::new(start, end).add_stop(0.0, VIOLET).add_stop(1.0, INDIGO);
                frame.stroke(
                    &ribbon,
                    Stroke {
                        style: canvas::Style::Gradient(gradient.into()),
                        width: 6.0,
                        line_cap: canvas::LineCap::Round,
                        ..Stroke::default()
                    },
                );
                // Light flows toward whichever screen has the cursor, on the ribbons that reach it.
                if self.mode != Mode::Local && (self.focus == Some(sa.id) || self.focus == Some(sb.id)) {
                    let dir = if self.focus == Some(sb.id) { 1.0 } else { -1.0 };
                    frame.stroke(
                        &ribbon,
                        Stroke {
                            style: canvas::Style::Solid(alpha(iced::Color::WHITE, 0.9)),
                            width: 2.5,
                            line_cap: canvas::LineCap::Round,
                            line_dash: canvas::LineDash {
                                segments: &[6.0, 14.0],
                                // Shifting the dash offset each frame makes the light travel; the sign sets its direction.
                                offset: (-dir * self.phase * 20.0).rem_euclid(20.0) as usize,
                            },
                            ..Stroke::default()
                        },
                    );
                }
            }
        }
        vec![frame.into_geometry()]
    }
}

/// Whether two screens share an edge segment in Desk coordinates (corners alone don't count).
fn touch(a: &ScreenView, b: &ScreenView) -> bool {
    let (ax, ay, bx, by) = (i64::from(a.x), i64::from(a.y), i64::from(b.x), i64::from(b.y));
    let (aw, ah, bw, bh) = (i64::from(a.w), i64::from(a.h), i64::from(b.w), i64::from(b.h));
    let overlap = |p: i64, pl: i64, q: i64, ql: i64| p.max(q) < (p + pl).min(q + ql);
    let side = (ax + aw == bx || bx + bw == ax) && overlap(ay, ah, by, bh);
    let stacked = (ay + ah == by || by + bh == ay) && overlap(ax, aw, bx, bw);
    side || stacked
}

/// Each screen's rectangle on a canvas of the given size, in the order given: the Desk scaled
/// to fit inside the padding, every screen inset by half the gap so neighbours stay `GAP`
/// apart. `None` when the Desk has no area.
fn layout(screens: &[ScreenView], width: f32, height: f32) -> Option<Vec<Rectangle>> {
    let min_x = screens.iter().map(|s| s.x as f32).fold(f32::INFINITY, f32::min);
    let min_y = screens.iter().map(|s| s.y as f32).fold(f32::INFINITY, f32::min);
    let max_x = screens
        .iter()
        .map(|s| s.x as f32 + s.w as f32)
        .fold(f32::NEG_INFINITY, f32::max);
    let max_y = screens
        .iter()
        .map(|s| s.y as f32 + s.h as f32)
        .fold(f32::NEG_INFINITY, f32::max);
    let (dx, dy) = (max_x - min_x, max_y - min_y);
    if !(dx > 0.0 && dy > 0.0) {
        return None;
    }
    // The insets eat half a gap on every outer edge, so the Desk may overshoot the padding by one gap.
    // A canvas too small for the padding clamps to zero instead of going negative.
    let scale = ((width - 2.0 * PAD + GAP) / dx)
        .min((height - 2.0 * PAD + GAP) / dy)
        .max(0.0);
    let (ox, oy) = ((width - dx * scale) / 2.0, (height - dy * scale) / 2.0);
    let half = GAP / 2.0;
    Some(
        screens
            .iter()
            .map(|s| {
                Rectangle::new(
                    Point::new(
                        ox + (s.x as f32 - min_x) * scale + half,
                        oy + (s.y as f32 - min_y) * scale + half,
                    ),
                    Size::new((s.w as f32 * scale - GAP).max(0.0), (s.h as f32 * scale - GAP).max(0.0)),
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: i32, y: i32, w: u32, h: u32) -> ScreenView {
        ScreenView {
            id: PeerId([0; 16]),
            name: String::new(),
            x,
            y,
            w,
            h,
            me: false,
        }
    }

    #[test]
    fn touching_needs_a_shared_edge_segment() {
        let a = screen(0, 0, 100, 100);
        assert!(touch(&a, &screen(100, 20, 50, 50)), "right-hand neighbour");
        assert!(touch(&a, &screen(-50, 90, 50, 50)), "left, overlapping by 10");
        assert!(touch(&a, &screen(10, 100, 50, 50)), "below");
        assert!(!touch(&a, &screen(100, 100, 50, 50)), "corner only");
        assert!(!touch(&a, &screen(150, 0, 50, 50)), "a gap apart");
    }

    #[test]
    fn inset_keeps_neighbours_a_gap_apart_inside_the_padding() {
        let screens = [screen(0, 0, 200, 100), screen(200, 0, 200, 100)];
        let r = layout(&screens, 400.0, 300.0).unwrap();
        assert!((r[1].x - (r[0].x + r[0].width) - GAP).abs() < 0.01);
        assert!(r[0].x >= PAD - 0.01 && r[1].x + r[1].width <= 400.0 - PAD + 0.01);
        assert!(layout(&[screen(0, 0, 0, 10)], 400.0, 300.0).is_none());
    }
}
