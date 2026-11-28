// SPDX-License-Identifier: AGPL-3.0-only
//! The Desk map: real screens to scale, joined by the brand ribbon. The screen with the
//! cursor glows teal; while crossing, light flows along the ribbon toward it.

use iced::Event;
use iced::mouse::{self, Cursor};
use iced::widget::canvas::{self, Frame, Geometry, Path, Program, Stroke, Text, gradient::Linear};
use iced::{Point, Rectangle, Renderer, Size, Theme, alignment};
use overdesk_proto::PeerId;
use overdeskd::Mode;
use overdeskd::controller::{Command, DeviceKind, Placement, ScreenView};

use crate::Message;

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
    /// Whether screens may be dragged to rearrange the Desk.
    pub can_arrange: bool,
}

/// A screen being dragged: whose it is, and where the cursor is on the canvas.
#[derive(Debug, Clone, Copy)]
pub struct Drag {
    /// The dragged screen's device.
    peer: PeerId,
    /// Where the press happened, relative to the canvas.
    from: Point,
    /// The cursor, relative to the canvas.
    at: Point,
}

/// The screens, focus and darkness a drawing was made for.
type DrawnFor = (Vec<ScreenView>, Option<PeerId>, bool);

/// What the map keeps between frames: a drag in progress, and the drawn screens and ribbons
/// with the inputs they were drawn from.
#[derive(Default)]
pub struct DeskState {
    /// The screen being dragged, if any.
    drag: Option<Drag>,
    /// The drawn screens and ribbons; redrawn when [`DeskState::key`] changes or the size does.
    cache: canvas::Cache,
    /// The screens, focus and darkness the cache was drawn for.
    key: std::cell::RefCell<Option<DrawnFor>>,
}

/// How far the cursor must travel before a press counts as a drag, not a click.
const DRAG_SLOP: f32 = 4.0;

/// The space between neighbouring screens, in pixels.
const GAP: f32 = 24.0;
/// The margin around the drawn Desk, in pixels.
const PAD: f32 = 20.0;

impl Program<Message> for DeskMap<'_> {
    type State = DeskState;

    fn update(
        &self,
        state: &mut DeskState,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Option<canvas::Action<Message>> {
        let state = &mut state.drag;
        if !self.can_arrange {
            *state = None; // arranging was switched off mid-drag
            return None;
        }
        // A drag cannot survive losing the window: its release would never arrive.
        if matches!(event, Event::Window(iced::window::Event::Unfocused)) {
            *state = None;
            return Some(canvas::Action::request_redraw());
        }
        let Event::Mouse(event) = event else { return None };
        match event {
            mouse::Event::CursorLeft => {
                *state = None;
                Some(canvas::Action::request_redraw())
            }
            mouse::Event::ButtonPressed(mouse::Button::Left) => {
                // A press starts afresh, whatever an earlier drag left behind.
                *state = None;
                let at = cursor.position_in(bounds)?;
                let rects = layout(self.screens, bounds.width, bounds.height)?;
                let hit = rects.iter().zip(self.screens).find(|(r, _)| r.contains(at))?.1;
                // The anchor is this device's screen; it never moves.
                if !hit.me {
                    *state = Some(Drag {
                        peer: hit.id,
                        from: at,
                        at,
                    });
                }
                Some(canvas::Action::request_redraw().and_capture())
            }
            mouse::Event::CursorMoved { .. } if state.is_some() => {
                // Off the canvas the last position stands, so the ghost stays put.
                if let (Some(d), Some(at)) = (state.as_mut(), cursor.position_in(bounds)) {
                    d.at = at;
                }
                Some(canvas::Action::request_redraw())
            }
            mouse::Event::ButtonReleased(mouse::Button::Left) => {
                let d = state.take()?;
                // A release off the map cancels; a click without a drag, or a screen that
                // left meanwhile, moves nothing.
                let at = cursor.position_in(bounds)?;
                if at.distance(d.from) < DRAG_SLOP || !self.screens.iter().any(|s| s.id == d.peer) {
                    return Some(canvas::Action::request_redraw());
                }
                let rects = layout(self.screens, bounds.width, bounds.height)?;
                let (anchor, rect) = self.screens.iter().zip(&rects).find(|(s, _)| s.me)?;
                let side = placement_for(*rect, at);
                Some(
                    canvas::Action::publish(Message::Send(Command::Place {
                        peer: d.peer,
                        anchor: anchor.id,
                        side,
                    }))
                    .and_capture(),
                )
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &DeskState,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _: Cursor,
    ) -> Vec<Geometry> {
        let Some(rects) = layout(self.screens, bounds.width, bounds.height) else {
            // An empty or degenerate Desk has nothing to scale; draw nothing rather than divide by zero.
            return Vec::new();
        };
        let rects: Vec<(Rectangle, &ScreenView)> = rects.into_iter().zip(self.screens).collect();
        let ribbons = ribbons(&rects);
        let dark = is_dark(theme);
        // The screens and ribbons change only with the Desk, the focus, the theme or the size:
        // redrawing them 30 times a second for the light that flows along them is waste.
        let same = state
            .key
            .borrow()
            .as_ref()
            .is_some_and(|(s, f, d)| s == self.screens && *f == self.focus && *d == dark);
        if !same {
            state.cache.clear();
            *state.key.borrow_mut() = Some((self.screens.to_vec(), self.focus, dark));
        }
        let fixed = state.cache.draw(renderer, bounds.size(), |frame| {
            for (r, s) in &rects {
                draw_screen(frame, theme, *r, s, self.focus == Some(s.id));
            }
            for ribbon in &ribbons {
                frame.stroke(
                    &ribbon.path,
                    Stroke {
                        style: canvas::Style::Gradient(
                            Linear::new(ribbon.start, ribbon.end)
                                .add_stop(0.0, VIOLET)
                                .add_stop(1.0, INDIGO)
                                .into(),
                        ),
                        width: 6.0,
                        line_cap: canvas::LineCap::Round,
                        ..Stroke::default()
                    },
                );
            }
        });
        let mut frame = Frame::new(renderer, bounds.size());
        // Light flows toward whichever screen has the cursor, on the ribbons that reach it.
        if self.mode != Mode::Local {
            for ribbon in &ribbons {
                let Some(dir) = ribbon.toward(self.focus) else { continue };
                frame.stroke(
                    &ribbon.path,
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
        if let Some(d) = &state.drag
            && let Some((r, _)) = rects.iter().find(|(_, s)| s.id == d.peer)
        {
            let ghost = Path::rounded_rectangle(
                Point::new(d.at.x - r.width / 2.0, d.at.y - r.height / 2.0),
                r.size(),
                12.0.into(),
            );
            frame.fill(&ghost, alpha(TEAL, 0.25));
            frame.stroke(&ghost, Stroke::default().with_color(TEAL).with_width(2.0));
        }
        vec![fixed, frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &DeskState, _: Rectangle, _: Cursor) -> mouse::Interaction {
        if state.drag.is_some() {
            mouse::Interaction::Grabbing
        } else {
            mouse::Interaction::default()
        }
    }
}

/// One screen: depth, bezel, glow when it has the cursor, face and its labels.
fn draw_screen(frame: &mut Frame, theme: &Theme, r: Rectangle, s: &ScreenView, on: bool) {
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
        &if on { bezel } else { face },
        Stroke::default()
            .with_color(if on { TEAL } else { hairline(theme) })
            .with_width(if on { 2.0 } else { 1.0 }),
    );
    draw_labels(frame, theme, r, s);
}

/// The name, size and (for a television) glyph of a screen. Labels shrink, shorten or drop so
/// they always stay inside their screen.
fn draw_labels(frame: &mut Frame, theme: &Theme, r: Rectangle, s: &ScreenView) {
    let pad = (r.width * 0.08).clamp(6.0, 14.0);
    let room = r.width - 2.0 * pad;
    let name_size = if r.height >= 44.0 { 14.0 } else { 11.0 };
    let name_font = iced::Font {
        weight: iced::font::Weight::Semibold,
        ..iced::Font::with_name("Inter")
    };
    // A screen whose device is away is dimmed.
    let colour = if s.online { ink(theme) } else { muted(theme) };
    if r.height >= name_size + 2.0 * 6.0 {
        frame.fill_text(Text {
            content: fit(&s.name, room, name_size, name_font),
            position: Point::new(r.x + pad, r.y + pad.min(14.0)),
            color: colour,
            size: name_size.into(),
            font: name_font,
            align_y: alignment::Vertical::Top,
            ..Text::default()
        });
    }
    // A television gets a small TV glyph in its corner.
    if matches!(s.kind, DeviceKind::Remote { .. }) && r.height >= 58.0 && room >= 80.0 {
        let tv = Rectangle::new(
            Point::new(r.x + r.width - pad - 22.0, r.y + pad.min(14.0)),
            Size::new(22.0, 15.0),
        );
        frame.stroke(
            &Path::rounded_rectangle(tv.position(), tv.size(), 3.0.into()),
            Stroke::default().with_color(colour).with_width(1.5),
        );
        let foot = Path::line(
            Point::new(tv.x + 5.0, tv.y + tv.height + 3.0),
            Point::new(tv.x + tv.width - 5.0, tv.y + tv.height + 3.0),
        );
        frame.stroke(&foot, Stroke::default().with_color(colour).with_width(1.5));
    }
    if r.height >= 58.0 {
        let detail = if s.me {
            "This device".to_string()
        } else if !s.online {
            "Offline".to_string()
        } else {
            format!("{} × {}", s.w, s.h)
        };
        frame.fill_text(Text {
            content: fit(&detail, room, 12.0, iced::Font::with_name("Inter")),
            position: Point::new(r.x + pad, r.y + pad.min(14.0) + 20.0),
            color: muted(theme),
            size: 12.0.into(),
            align_y: alignment::Vertical::Top,
            ..Text::default()
        });
    }
}

/// A ribbon across an edge two screens share.
struct Ribbon {
    /// The curve from the first screen to the second.
    path: Path,
    /// Where it starts.
    start: Point,
    /// Where it ends.
    end: Point,
    /// The screen it starts in: the left-hand or upper one.
    from: PeerId,
    /// The screen it ends in: the right-hand or lower one.
    to: PeerId,
}

impl Ribbon {
    /// Which way light flows when `focus` has the cursor: 1 toward `to`, -1 toward `from`,
    /// `None` for a ribbon that does not reach the focus.
    fn toward(&self, focus: Option<PeerId>) -> Option<f32> {
        match focus {
            Some(f) if f == self.to => Some(1.0),
            Some(f) if f == self.from => Some(-1.0),
            _ => None,
        }
    }
}

/// A ribbon across every edge two screens share.
fn ribbons(rects: &[(Rectangle, &ScreenView)]) -> Vec<Ribbon> {
    let mut out = Vec::new();
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
            let path = Path::new(|p| {
                p.move_to(start);
                if horizontal {
                    let mid = (a.x + a.width + b.x) / 2.0;
                    p.bezier_curve_to(Point::new(mid, start.y), Point::new(mid, end.y), end);
                } else {
                    let mid = (a.y + a.height + b.y) / 2.0;
                    p.bezier_curve_to(Point::new(start.x, mid), Point::new(end.x, mid), end);
                }
            });
            out.push(Ribbon {
                path,
                start,
                end,
                from: sa.id,
                to: sb.id,
            });
        }
    }
    out
}

/// Which side of the `anchor` screen a drop at `at` lands on: the axis the drop is furthest along,
/// measured in half-screens so wide and tall screens both get fair quadrants.
fn placement_for(anchor: Rectangle, at: Point) -> Placement {
    let c = anchor.center();
    let (dx, dy) = (
        (at.x - c.x) / anchor.width.max(1.0),
        (at.y - c.y) / anchor.height.max(1.0),
    );
    match (dx.abs() >= dy.abs(), dx < 0.0, dy < 0.0) {
        (true, true, _) => Placement::Left,
        (true, false, _) => Placement::Right,
        (false, _, true) => Placement::Above,
        (false, _, false) => Placement::Below,
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

/// Width in pixels of `text` on one line in `font` at `size`, as the renderer will lay it out.
fn measure(text: &str, size: f32, font: iced::Font) -> f32 {
    use iced::advanced::text::Paragraph as _;
    iced::advanced::graphics::text::Paragraph::with_text(iced::advanced::text::Text {
        content: text,
        bounds: Size::INFINITE,
        size: size.into(),
        line_height: Default::default(),
        font,
        align_x: iced::advanced::text::Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: Default::default(),
        wrapping: iced::advanced::text::Wrapping::None,
    })
    .min_bounds()
    .width
}

thread_local! {
    /// Results of [`fit`] by text, width, size and font, so resizing the window or redrawing
    /// the same labels does not measure them again. Emptied when it grows past a few dozen.
    static FITTED: std::cell::RefCell<std::collections::HashMap<(String, u32, u32, iced::Font), String>> =
        std::cell::RefCell::default();
}

/// [`shorten`], remembered.
fn fit(text: &str, width: f32, size: f32, font: iced::Font) -> String {
    let key = (text.to_owned(), width.to_bits(), size.to_bits(), font);
    if let Some(done) = FITTED.with_borrow(|m| m.get(&key).cloned()) {
        return done;
    }
    let done = shorten(text, width, size, font);
    FITTED.with_borrow_mut(|m| {
        if m.len() >= 64 {
            m.clear();
        }
        m.insert(key, done.clone());
    });
    done
}

/// `text` shortened with an ellipsis to fit `width` pixels at font `size`: the longest
/// prefix that fits once measured, or empty if not even the ellipsis does.
fn shorten(text: &str, width: f32, size: f32, font: iced::Font) -> String {
    if measure(text, size, font) <= width {
        return text.to_string();
    }
    let ends: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let cut = |n: usize| format!("{}…", &text[..ends[n]]);
    // Widths grow with the prefix, so the longest fitting one is found by bisection. The
    // whole text does not fit, so the longest candidate drops at least its last character
    // (and `ends[n]` stays in bounds).
    let (mut lo, mut hi) = (0, ends.len().saturating_sub(1));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&cut(mid), size, font) <= width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 && measure("…", size, font) > width {
        return String::new();
    }
    cut(lo)
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
            online: true,
            kind: overdeskd::controller::DeviceKind::Computer,
        }
    }

    /// An arrangeable map with no screens, enough to feed `update` events.
    fn map() -> DeskMap<'static> {
        DeskMap {
            screens: &[],
            focus: None,
            mode: Mode::Local,
            phase: 0.0,
            can_arrange: true,
        }
    }

    #[test]
    fn a_drag_ends_when_the_cursor_leaves_or_the_window_loses_focus() {
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(200.0, 100.0));
        let drag = Drag {
            peer: PeerId([1; 16]),
            from: Point::ORIGIN,
            at: Point::ORIGIN,
        };
        let events = [
            Event::Mouse(mouse::Event::CursorLeft),
            Event::Window(iced::window::Event::Unfocused),
            // A press that hits nothing still starts afresh.
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ];
        for event in events {
            let mut state = DeskState {
                drag: Some(drag),
                ..DeskState::default()
            };
            map().update(&mut state, &event, bounds, Cursor::Unavailable);
            assert!(state.drag.is_none(), "{event:?}");
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

    #[test]
    fn drop_side_follows_the_dominant_axis_from_the_anchor_centre() {
        let a = Rectangle::new(Point::new(100.0, 100.0), Size::new(200.0, 100.0));
        assert_eq!(placement_for(a, Point::new(0.0, 150.0)), Placement::Left);
        assert_eq!(placement_for(a, Point::new(400.0, 140.0)), Placement::Right);
        assert_eq!(placement_for(a, Point::new(210.0, 0.0)), Placement::Above);
        assert_eq!(placement_for(a, Point::new(190.0, 300.0)), Placement::Below);
        // 140 px right but 90 px down: the drop is further along the vertical in screen-sizes.
        assert_eq!(placement_for(a, Point::new(340.0, 240.0)), Placement::Below);
    }

    #[test]
    fn labels_are_shortened_to_fit() {
        let font = iced::Font::with_name("Inter");
        assert_eq!(fit("fedora", 200.0, 14.0, font), "fedora");
        let name = "DESKTOP-7Q2K4LM-WITH-A-VERY-LONG-HOSTNAME";
        let short = fit(name, 90.0, 14.0, font);
        assert!(short.ends_with('…') && short.len() < name.len(), "{short}");
        assert!(measure(&short, 14.0, font) <= 90.0, "{short}");
        // One more character would no longer fit: the cut is the longest.
        let n = short.chars().count();
        let longer = format!("{}…", name.chars().take(n).collect::<String>());
        assert!(measure(&longer, 14.0, font) > 90.0, "{short}");
        assert_eq!(fit("anything", 5.0, 14.0, font), "");
    }

    #[test]
    fn a_label_just_too_wide_keeps_all_but_its_end() {
        let font = iced::Font::default();
        let name = "A computer name chosen by the peer";
        let full = measure(name, 14.0, font);
        // Every shorter prefix fits: the bisection must still never index past the text.
        let out = fit(name, full - 0.01, 14.0, font);
        assert!(out.ends_with('…') && out.chars().count() <= name.chars().count());
        assert_eq!(fit("", -1.0, 14.0, font), "");
    }
}
