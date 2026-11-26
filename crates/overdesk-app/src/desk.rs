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
        let min_x = self.screens.iter().map(|s| s.x).min().unwrap_or(0) as f32;
        let min_y = self.screens.iter().map(|s| s.y).min().unwrap_or(0) as f32;
        let max_x = self
            .screens
            .iter()
            .map(|s| s.x as f32 + s.w as f32)
            .fold(f32::NEG_INFINITY, f32::max);
        let max_y = self
            .screens
            .iter()
            .map(|s| s.y as f32 + s.h as f32)
            .fold(f32::NEG_INFINITY, f32::max);
        // A degenerate Desk has nothing to scale; draw nothing rather than divide by zero.
        if max_x - min_x <= 0.0 || max_y - min_y <= 0.0 {
            return vec![frame.into_geometry()];
        }
        let gaps = (self.screens.len() - 1) as f32 * GAP;
        // Fit the whole Desk inside the padded bounds, keeping the screens' proportions.
        // A canvas too small for the padding and gaps clamps to zero instead of going negative.
        let scale = ((bounds.width - 2.0 * PAD - gaps) / (max_x - min_x))
            .min((bounds.height - 2.0 * PAD) / (max_y - min_y))
            .max(0.0);
        let drawn_w = (max_x - min_x) * scale + gaps;
        let ox = (bounds.width - drawn_w) / 2.0;
        let oy = (bounds.height - (max_y - min_y) * scale) / 2.0;

        // Lay the screens out left to right, so a fixed gap can separate neighbours.
        let mut ordered: Vec<&ScreenView> = self.screens.iter().collect();
        ordered.sort_by_key(|s| s.x);
        let rects: Vec<(Rectangle, &ScreenView)> = ordered
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let r = Rectangle::new(
                    Point::new(
                        ox + (s.x as f32 - min_x) * scale + i as f32 * GAP,
                        oy + (s.y as f32 - min_y) * scale,
                    ),
                    Size::new(s.w as f32 * scale, s.h as f32 * scale),
                );
                (r, *s)
            })
            .collect();

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

        // The ribbon: from low in the left screen, up through the gap, into the right one.
        if let [(a, _), (b, sb), ..] = rects.as_slice() {
            // Starts low in the left screen and ends low in the right one, clear of the labels.
            let start = Point::new(a.x + a.width * 0.5, a.y + a.height * 0.82);
            let end = Point::new(b.x + b.width * 0.5, b.y + b.height * 0.72);
            let mid = (a.x + a.width + b.x) / 2.0;
            let ribbon = Path::new(|p| {
                p.move_to(start);
                p.bezier_curve_to(Point::new(mid, start.y), Point::new(mid, end.y), end);
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
            if self.mode != Mode::Local {
                // Light flows toward whichever screen has the cursor.
                // The screens are sorted left to right, so `b` is the right-hand one.
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
        vec![frame.into_geometry()]
    }
}
