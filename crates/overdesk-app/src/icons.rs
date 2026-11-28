// SPDX-License-Identifier: AGPL-3.0-only
//! Operating-system logos beside device names, drawn as canvas paths in the text colour.
//!
//! The marks are the Simple Icons paths (<https://simpleicons.org>, released under CC0), the
//! same ones the website uses, on a 24x24 viewBox. The logos are trademarks of their owners;
//! they only identify the systems Overdesk runs on or controls. A small SVG path parser turns
//! each `d` string into canvas path commands once, so the app needs no SVG renderer.

use std::sync::OnceLock;

use iced::widget::canvas::{Frame, Geometry, Path, Program};
use iced::{Point, Rectangle, Renderer, Theme, mouse};
use overdesk_proto::Os;
use overdeskd::controller::DeviceKind;

use crate::style::ink;

/// Simple Icons (CC0) path data for the apple mark.
const APPLE: &str = r#"M12.152 6.896c-.948 0-2.415-1.078-3.96-1.04-2.04.027-3.91 1.183-4.961 3.014-2.117 3.675-.546 9.103 1.519 12.09 1.013 1.454 2.208 3.09 3.792 3.039 1.52-.065 2.09-.987 3.935-.987 1.831 0 2.35.987 3.96.948 1.637-.026 2.676-1.48 3.676-2.948 1.156-1.688 1.636-3.325 1.662-3.415-.039-.013-3.182-1.221-3.22-4.857-.026-3.04 2.48-4.494 2.597-4.559-1.429-2.09-3.623-2.324-4.39-2.376-2-.156-3.675 1.09-4.61 1.09zM15.53 3.83c.843-1.012 1.4-2.427 1.245-3.83-1.207.052-2.662.805-3.532 1.818-.78.896-1.454 2.338-1.273 3.714 1.338.104 2.715-.688 3.559-1.701"#;
/// Simple Icons (CC0) path data for the windows mark.
const WINDOWS: &str = r#"M0,0H11.377V11.372H0ZM12.623,0H24V11.372H12.623ZM0,12.623H11.377V24H0Zm12.623,0H24V24H12.623"#;
/// Simple Icons (CC0) path data for the linux mark.
const LINUX: &str = r#"M12.504 0c-.155 0-.315.008-.48.021-4.226.333-3.105 4.807-3.17 6.298-.076 1.092-.3 1.953-1.05 3.02-.885 1.051-2.127 2.75-2.716 4.521-.278.832-.41 1.684-.287 2.489a.424.424 0 00-.11.135c-.26.268-.45.6-.663.839-.199.199-.485.267-.797.4-.313.136-.658.269-.864.68-.09.189-.136.394-.132.602 0 .199.027.4.055.536.058.399.116.728.04.97-.249.68-.28 1.145-.106 1.484.174.334.535.47.94.601.81.2 1.91.135 2.774.6.926.466 1.866.67 2.616.47.526-.116.97-.464 1.208-.946.587-.003 1.23-.269 2.26-.334.699-.058 1.574.267 2.577.2.025.134.063.198.114.333l.003.003c.391.778 1.113 1.132 1.884 1.071.771-.06 1.592-.536 2.257-1.306.631-.765 1.683-1.084 2.378-1.503.348-.199.629-.469.649-.853.023-.4-.2-.811-.714-1.376v-.097l-.003-.003c-.17-.2-.25-.535-.338-.926-.085-.401-.182-.786-.492-1.046h-.003c-.059-.054-.123-.067-.188-.135a.357.357 0 00-.19-.064c.431-1.278.264-2.55-.173-3.694-.533-1.41-1.465-2.638-2.175-3.483-.796-1.005-1.576-1.957-1.56-3.368.026-2.152.236-6.133-3.544-6.139zm.529 3.405h.013c.213 0 .396.062.584.198.19.135.33.332.438.533.105.259.158.459.166.724 0-.02.006-.04.006-.06v.105a.086.086 0 01-.004-.021l-.004-.024a1.807 1.807 0 01-.15.706.953.953 0 01-.213.335.71.71 0 00-.088-.042c-.104-.045-.198-.064-.284-.133a1.312 1.312 0 00-.22-.066c.05-.06.146-.133.183-.198.053-.128.082-.264.088-.402v-.02a1.21 1.21 0 00-.061-.4c-.045-.134-.101-.2-.183-.333-.084-.066-.167-.132-.267-.132h-.016c-.093 0-.176.03-.262.132a.8.8 0 00-.205.334 1.18 1.18 0 00-.09.4v.019c.002.089.008.179.02.267-.193-.067-.438-.135-.607-.202a1.635 1.635 0 01-.018-.2v-.02a1.772 1.772 0 01.15-.768c.082-.22.232-.406.43-.533a.985.985 0 01.594-.2zm-2.962.059h.036c.142 0 .27.048.399.135.146.129.264.288.344.465.09.199.14.4.153.667v.004c.007.134.006.2-.002.266v.08c-.03.007-.056.018-.083.024-.152.055-.274.135-.393.2.012-.09.013-.18.003-.267v-.015c-.012-.133-.04-.2-.082-.333a.613.613 0 00-.166-.267.248.248 0 00-.183-.064h-.021c-.071.006-.13.04-.186.132a.552.552 0 00-.12.27.944.944 0 00-.023.33v.015c.012.135.037.2.08.334.046.134.098.2.166.268.01.009.02.018.034.024-.07.057-.117.07-.176.136a.304.304 0 01-.131.068 2.62 2.62 0 01-.275-.402 1.772 1.772 0 01-.155-.667 1.759 1.759 0 01.08-.668 1.43 1.43 0 01.283-.535c.128-.133.26-.2.418-.2zm1.37 1.706c.332 0 .733.065 1.216.399.293.2.523.269 1.052.468h.003c.255.136.405.266.478.399v-.131a.571.571 0 01.016.47c-.123.31-.516.643-1.063.842v.002c-.268.135-.501.333-.775.465-.276.135-.588.292-1.012.267a1.139 1.139 0 01-.448-.067 3.566 3.566 0 01-.322-.198c-.195-.135-.363-.332-.612-.465v-.005h-.005c-.4-.246-.616-.512-.686-.71-.07-.268-.005-.47.193-.6.224-.135.38-.271.483-.336.104-.074.143-.102.176-.131h.002v-.003c.169-.202.436-.47.839-.601.139-.036.294-.065.466-.065zm2.8 2.142c.358 1.417 1.196 3.475 1.735 4.473.286.534.855 1.659 1.102 3.024.156-.005.33.018.513.064.646-1.671-.546-3.467-1.089-3.966-.22-.2-.232-.335-.123-.335.59.534 1.365 1.572 1.646 2.757.13.535.16 1.104.021 1.67.067.028.135.06.205.067 1.032.534 1.413.938 1.23 1.537v-.043c-.06-.003-.12 0-.18 0h-.016c.151-.467-.182-.825-1.065-1.224-.915-.4-1.646-.336-1.77.465-.008.043-.013.066-.018.135-.068.023-.139.053-.209.064-.43.268-.662.669-.793 1.187-.13.533-.17 1.156-.205 1.869v.003c-.02.334-.17.838-.319 1.35-1.5 1.072-3.58 1.538-5.348.334a2.645 2.645 0 00-.402-.533 1.45 1.45 0 00-.275-.333c.182 0 .338-.03.465-.067a.615.615 0 00.314-.334c.108-.267 0-.697-.345-1.163-.345-.467-.931-.995-1.788-1.521-.63-.4-.986-.87-1.15-1.396-.165-.534-.143-1.085-.015-1.645.245-1.07.873-2.11 1.274-2.763.107-.065.037.135-.408.974-.396.751-1.14 2.497-.122 3.854a8.123 8.123 0 01.647-2.876c.564-1.278 1.743-3.504 1.836-5.268.048.036.217.135.289.202.218.133.38.333.59.465.21.201.477.335.876.335.039.003.075.006.11.006.412 0 .73-.134.997-.268.29-.134.52-.334.74-.4h.005c.467-.135.835-.402 1.044-.7zm2.185 8.958c.037.6.343 1.245.882 1.377.588.134 1.434-.333 1.791-.765l.211-.01c.315-.007.577.01.847.268l.003.003c.208.199.305.53.391.876.085.4.154.78.409 1.066.486.527.645.906.636 1.14l.003-.007v.018l-.003-.012c-.015.262-.185.396-.498.595-.63.401-1.746.712-2.457 1.57-.618.737-1.37 1.14-2.036 1.191-.664.053-1.237-.2-1.574-.898l-.005-.003c-.21-.4-.12-1.025.056-1.69.176-.668.428-1.344.463-1.897.037-.714.076-1.335.195-1.814.12-.465.308-.797.641-.984l.045-.022zm-10.814.049h.01c.053 0 .105.005.157.014.376.055.706.333 1.023.752l.91 1.664.003.003c.243.533.754 1.064 1.189 1.637.434.598.77 1.131.729 1.57v.006c-.057.744-.48 1.148-1.125 1.294-.645.135-1.52.002-2.395-.464-.968-.536-2.118-.469-2.857-.602-.369-.066-.61-.2-.723-.4-.11-.2-.113-.602.123-1.23v-.004l.002-.003c.117-.334.03-.752-.027-1.118-.055-.401-.083-.71.043-.94.16-.334.396-.4.69-.533.294-.135.64-.202.915-.47h.002v-.002c.256-.268.445-.601.668-.838.19-.201.38-.336.663-.336zm7.159-9.074c-.435.201-.945.535-1.488.535-.542 0-.97-.267-1.28-.466-.154-.134-.28-.268-.373-.335-.164-.134-.144-.333-.074-.333.109.016.129.134.199.2.096.066.215.2.36.333.292.2.68.467 1.167.467.485 0 1.053-.267 1.398-.466.195-.135.445-.334.648-.467.156-.136.149-.267.279-.267.128.016.034.134-.147.332a8.097 8.097 0 01-.69.468zm-1.082-1.583V5.64c-.006-.02.013-.042.029-.05.074-.043.18-.027.26.004.063 0 .16.067.15.135-.006.049-.085.066-.135.066-.055 0-.092-.043-.141-.068-.052-.018-.146-.008-.163-.065zm-.551 0c-.02.058-.113.049-.166.066-.047.025-.086.068-.14.068-.05 0-.13-.02-.136-.068-.01-.066.088-.133.15-.133.08-.031.184-.047.259-.005.019.009.036.03.03.05v.02h.003z"#;
/// Simple Icons (CC0) path data for the android mark.
const ANDROID: &str = r#"M18.4395 5.5586c-.675 1.1664-1.352 2.3318-2.0274 3.498-.0366-.0155-.0742-.0286-.1113-.043-1.8249-.6957-3.484-.8-4.42-.787-1.8551.0185-3.3544.4643-4.2597.8203-.084-.1494-1.7526-3.021-2.0215-3.4864a1.1451 1.1451 0 0 0-.1406-.1914c-.3312-.364-.9054-.4859-1.379-.203-.475.282-.7136.9361-.3886 1.5019 1.9466 3.3696-.0966-.2158 1.9473 3.3593.0172.031-.4946.2642-1.3926 1.0177C2.8987 12.176.452 14.772 0 18.9902h24c-.119-1.1108-.3686-2.099-.7461-3.0683-.7438-1.9118-1.8435-3.2928-2.7402-4.1836a12.1048 12.1048 0 0 0-2.1309-1.6875c.6594-1.122 1.312-2.2559 1.9649-3.3848.2077-.3615.1886-.7956-.0079-1.1191a1.1001 1.1001 0 0 0-.8515-.5332c-.5225-.0536-.9392.3128-1.0488.5449zm-.0391 8.461c.3944.5926.324 1.3306-.1563 1.6503-.4799.3197-1.188.0985-1.582-.4941-.3944-.5927-.324-1.3307.1563-1.6504.4727-.315 1.1812-.1086 1.582.4941zM7.207 13.5273c.4803.3197.5506 1.0577.1563 1.6504-.394.5926-1.1038.8138-1.584.4941-.48-.3197-.5503-1.0577-.1563-1.6504.4008-.6021 1.1087-.8106 1.584-.4941z"#;

/// Which mark to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Icon {
    /// Apple: a Mac, and an Apple TV.
    Apple,
    /// The Windows flag.
    Windows,
    /// Tux.
    Linux,
    /// The Android robot: an Android TV.
    Android,
}

impl Icon {
    /// The mark for a device: a remote device (Pro lists Apple TVs) is Apple whatever OS its
    /// placeholder carries; a computer follows its OS; no OS, no mark. The BSDs have no
    /// [`Os`] of their own, so they keep whatever the build reports.
    pub(crate) fn of(kind: DeviceKind, os: Option<Os>) -> Option<Self> {
        match (kind, os) {
            (DeviceKind::Remote { .. }, _) => Some(Self::Apple),
            (DeviceKind::Computer, Some(Os::MacOs)) => Some(Self::Apple),
            (DeviceKind::Computer, Some(Os::Windows)) => Some(Self::Windows),
            (DeviceKind::Computer, Some(Os::Linux)) => Some(Self::Linux),
            (DeviceKind::Computer, Some(Os::Android)) => Some(Self::Android),
            (DeviceKind::Computer, None) => None,
        }
    }

    /// The parsed path, built on first use.
    fn segments(self) -> &'static [Seg] {
        static CACHE: [OnceLock<Vec<Seg>>; 4] = [const { OnceLock::new() }; 4];
        let (slot, d) = match self {
            Self::Apple => (0, APPLE),
            Self::Windows => (1, WINDOWS),
            Self::Linux => (2, LINUX),
            Self::Android => (3, ANDROID),
        };
        // The embedded paths are tested to parse; an empty mark beats a panic if one ever did not.
        CACHE[slot].get_or_init(|| parse_path(d).unwrap_or_default())
    }
}

/// A logo on a square canvas, filled with the theme's text colour.
pub(crate) struct Logo(pub(crate) Icon);

impl<Message> Program<Message> for Logo {
    type State = ();

    fn draw(&self, _: &(), renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        frame.scale(bounds.width.min(bounds.height) / 24.0);
        let path = Path::new(|b| {
            for seg in self.0.segments() {
                match *seg {
                    Seg::Move(p) => b.move_to(pt(p)),
                    Seg::Line(p) => b.line_to(pt(p)),
                    Seg::Quad(c, p) => b.quadratic_curve_to(pt(c), pt(p)),
                    Seg::Cubic(c1, c2, p) => b.bezier_curve_to(pt(c1), pt(c2), pt(p)),
                    Seg::Close => b.close(),
                }
            }
        });
        // Non-zero is the canvas default, which is what Simple Icons paths are drawn for.
        frame.fill(&path, ink(theme));
        vec![frame.into_geometry()]
    }
}

/// A point as a canvas point.
fn pt((x, y): P) -> Point {
    Point::new(x, y)
}

/// A point in the 24x24 box.
type P = (f32, f32);

/// One absolute path command; shorthand, relative and arc forms are resolved by [`parse_path`].
#[derive(Debug, Clone, Copy, PartialEq)]
enum Seg {
    /// Start a subpath.
    Move(P),
    /// A straight line.
    Line(P),
    /// A quadratic curve: control, end.
    Quad(P, P),
    /// A cubic curve: two controls, end.
    Cubic(P, P, P),
    /// Close the subpath.
    Close,
}

/// Why a path string did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathError(&'static str);

/// A cursor over path text.
struct Scan<'a> {
    /// The text as bytes.
    b: &'a [u8],
    /// The next byte.
    i: usize,
}

impl Scan<'_> {
    /// Skips whitespace and commas.
    fn skip(&mut self) {
        while self
            .b
            .get(self.i)
            .is_some_and(|c| c.is_ascii_whitespace() || *c == b',')
        {
            self.i += 1;
        }
    }

    /// Whether a number starts here.
    fn at_number(&self) -> bool {
        self.b
            .get(self.i)
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+'))
    }

    /// Reads a number: `1`, `-.5`, `+2.`, `1e-3`; in `1.5.5` the second dot starts a new one.
    fn number(&mut self) -> Result<f32, PathError> {
        self.skip();
        let start = self.i;
        let digits = |s: &mut Self| {
            let from = s.i;
            while s.b.get(s.i).is_some_and(u8::is_ascii_digit) {
                s.i += 1;
            }
            s.i - from
        };
        if matches!(self.b.get(self.i), Some(b'-' | b'+')) {
            self.i += 1;
        }
        let mut n = digits(self);
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            n += digits(self);
        }
        if n == 0 {
            return Err(PathError("number expected"));
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            let mark = self.i;
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'-' | b'+')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                self.i = mark;
            }
        }
        let v: f32 = std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or(PathError("bad number"))?;
        if v.is_finite() {
            Ok(v)
        } else {
            Err(PathError("bad number"))
        }
    }

    /// Reads an arc flag: one `0` or `1`, which may be glued to the next number.
    fn flag(&mut self) -> Result<bool, PathError> {
        self.skip();
        let f = match self.b.get(self.i) {
            Some(b'0') => false,
            Some(b'1') => true,
            _ => return Err(PathError("flag expected")),
        };
        self.i += 1;
        Ok(f)
    }

    /// Reads a coordinate pair, relative to `origin` when `rel`.
    fn point(&mut self, rel: bool, origin: P) -> Result<P, PathError> {
        let (x, y) = (self.number()?, self.number()?);
        Ok(if rel { (origin.0 + x, origin.1 + y) } else { (x, y) })
    }
}

/// Parses SVG path data (`M L H V C S Q T A Z`, upper and lower case, implicit repeats) into
/// absolute commands, with arcs turned into cubic curves.
fn parse_path(d: &str) -> Result<Vec<Seg>, PathError> {
    let mut s = Scan { b: d.as_bytes(), i: 0 };
    let mut out = Vec::new();
    let (mut cur, mut start): (P, P) = ((0.0, 0.0), (0.0, 0.0));
    // The last curve's control point, to reflect for S and T.
    let (mut last_c, mut last_q): (Option<P>, Option<P>) = (None, None);
    let mut cmd: Option<u8> = None;
    let mut closed = false;
    loop {
        s.skip();
        let Some(&c) = s.b.get(s.i) else { break };
        if c.is_ascii_alphabetic() {
            s.i += 1;
            cmd = Some(c);
        } else if !s.at_number() {
            return Err(PathError("unexpected character"));
        } else if let Some(prev) = cmd {
            // Further coordinates repeat the command; after a moveto they are linetos.
            cmd = match prev {
                b'M' => Some(b'L'),
                b'm' => Some(b'l'),
                b'Z' | b'z' => return Err(PathError("coordinates after close")),
                other => Some(other),
            };
        }
        let Some(cm) = cmd else {
            return Err(PathError("path must start with a command"));
        };
        if out.is_empty() && !matches!(cm, b'M' | b'm') {
            return Err(PathError("path must start with a moveto"));
        }
        let rel = cm.is_ascii_lowercase();
        let (mut nc, mut nq) = (None, None);
        // A drawing command after a close starts a new subpath at the old start.
        if closed && !matches!(cm, b'M' | b'm' | b'Z' | b'z') {
            out.push(Seg::Move(cur));
        }
        match cm.to_ascii_uppercase() {
            b'M' => {
                cur = s.point(rel, cur)?;
                start = cur;
                out.push(Seg::Move(cur));
            }
            b'L' => {
                cur = s.point(rel, cur)?;
                out.push(Seg::Line(cur));
            }
            b'H' => {
                let x = s.number()?;
                cur.0 = if rel { cur.0 + x } else { x };
                out.push(Seg::Line(cur));
            }
            b'V' => {
                let y = s.number()?;
                cur.1 = if rel { cur.1 + y } else { y };
                out.push(Seg::Line(cur));
            }
            b'C' => {
                let (c1, c2, p) = (s.point(rel, cur)?, s.point(rel, cur)?, s.point(rel, cur)?);
                out.push(Seg::Cubic(c1, c2, p));
                (cur, nc) = (p, Some(c2));
            }
            b'S' => {
                let c1 = last_c.map_or(cur, |l| (2.0 * cur.0 - l.0, 2.0 * cur.1 - l.1));
                let (c2, p) = (s.point(rel, cur)?, s.point(rel, cur)?);
                out.push(Seg::Cubic(c1, c2, p));
                (cur, nc) = (p, Some(c2));
            }
            b'Q' => {
                let (c1, p) = (s.point(rel, cur)?, s.point(rel, cur)?);
                out.push(Seg::Quad(c1, p));
                (cur, nq) = (p, Some(c1));
            }
            b'T' => {
                let c1 = last_q.map_or(cur, |l| (2.0 * cur.0 - l.0, 2.0 * cur.1 - l.1));
                let p = s.point(rel, cur)?;
                out.push(Seg::Quad(c1, p));
                (cur, nq) = (p, Some(c1));
            }
            b'A' => {
                let (rx, ry, rot) = (s.number()?, s.number()?, s.number()?);
                let (large, sweep) = (s.flag()?, s.flag()?);
                let p = s.point(rel, cur)?;
                arc(&mut out, cur, (rx, ry, rot), (large, sweep), p);
                cur = p;
            }
            b'Z' => {
                out.push(Seg::Close);
                cur = start;
                closed = true;
                (last_c, last_q) = (None, None);
                continue;
            }
            _ => return Err(PathError("unknown command")),
        }
        closed = false;
        (last_c, last_q) = (nc, nq);
    }
    Ok(out)
}

/// Appends an elliptical arc from `from` to `to` as cubic curves (SVG implementation notes,
/// F.6.5: endpoint to centre parameterisation, then at most a quarter turn per cubic).
fn arc(out: &mut Vec<Seg>, from: P, (rx, ry, rot): (f32, f32, f32), (large, sweep): (bool, bool), to: P) {
    let (x1, y1, x2, y2) = (f64::from(from.0), f64::from(from.1), f64::from(to.0), f64::from(to.1));
    let (mut rx, mut ry) = (f64::from(rx).abs(), f64::from(ry).abs());
    if (x1 == x2 && y1 == y2) || rx == 0.0 || ry == 0.0 {
        // Degenerate arcs are straight lines (or nothing).
        if x1 != x2 || y1 != y2 {
            out.push(Seg::Line(to));
        }
        return;
    }
    let phi = f64::from(rot).to_radians();
    let (sp, cp) = phi.sin_cos();
    let (dx, dy) = ((x1 - x2) / 2.0, (y1 - y2) / 2.0);
    let (x1p, y1p) = (cp * dx + sp * dy, -sp * dx + cp * dy);
    let lambda = (x1p / rx).powi(2) + (y1p / ry).powi(2);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = (rx * ry).powi(2) - (rx * y1p).powi(2) - (ry * x1p).powi(2);
    let den = (rx * y1p).powi(2) + (ry * x1p).powi(2);
    let mut k = (num / den).max(0.0).sqrt();
    if large == sweep {
        k = -k;
    }
    let (cxp, cyp) = (k * rx * y1p / ry, -k * ry * x1p / rx);
    let (cx, cy) = (
        cp * cxp - sp * cyp + (x1 + x2) / 2.0,
        sp * cxp + cp * cyp + (y1 + y2) / 2.0,
    );
    let angle = |ux: f64, uy: f64, vx: f64, vy: f64| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let theta1 = angle(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut delta = angle((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    let tau = std::f64::consts::TAU;
    if !sweep && delta > 0.0 {
        delta -= tau;
    } else if sweep && delta < 0.0 {
        delta += tau;
    }
    let n = (delta.abs() / std::f64::consts::FRAC_PI_2 - 1e-9).ceil().max(1.0);
    let step = delta / n;
    let t = 4.0 / 3.0 * (step / 4.0).tan();
    // A point on the unit-circle arc at angle `a`, mapped onto the ellipse.
    let map = |ux: f64, uy: f64| {
        (
            (cp * rx * ux - sp * ry * uy + cx) as f32,
            (sp * rx * ux + cp * ry * uy + cy) as f32,
        )
    };
    let mut a = theta1;
    for i in 0..n as usize {
        let (a2, (s1, c1), (s2, c2)) = (a + step, a.sin_cos(), (a + step).sin_cos());
        let end = if i + 1 == n as usize { to } else { map(c2, s2) };
        out.push(Seg::Cubic(
            map(c1 - t * s1, s1 + t * c1),
            map(c2 + t * s2, s2 - t * c2),
            end,
        ));
        a = a2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 4] = [Icon::Apple, Icon::Windows, Icon::Linux, Icon::Android];

    fn close(a: P, b: P) -> bool {
        (a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4
    }

    /// The end point of the last drawing command.
    fn last(segs: &[Seg]) -> P {
        match segs.iter().rev().find(|s| !matches!(s, Seg::Close)) {
            Some(Seg::Move(p) | Seg::Line(p) | Seg::Quad(_, p) | Seg::Cubic(_, _, p)) => *p,
            _ => panic!("no drawing command"),
        }
    }

    #[test]
    fn number_formats() {
        let s = parse_path("M.5-.5L1.5.5 1e-3,-2E+1 +3.-4.").unwrap();
        assert_eq!(s[0], Seg::Move((0.5, -0.5)));
        assert_eq!(s[1], Seg::Line((1.5, 0.5)));
        assert_eq!(s[2], Seg::Line((0.001, -20.0)));
        assert_eq!(s[3], Seg::Line((3.0, -4.0)));
    }

    #[test]
    fn glued_dots_split_into_numbers() {
        // "1.5.5" is 1.5 then .5
        assert_eq!(parse_path("M1.5.5").unwrap(), vec![Seg::Move((1.5, 0.5))]);
        // an `e` with no digits after it is not an exponent
        assert!(parse_path("M1e").is_err());
    }

    #[test]
    fn implicit_repeats_and_relative_moves() {
        let s = parse_path("m1 1 2 0 0 2z").unwrap();
        assert_eq!(s[0], Seg::Move((1.0, 1.0)));
        assert_eq!(s[1], Seg::Line((3.0, 1.0)));
        assert_eq!(s[2], Seg::Line((3.0, 3.0)));
        assert_eq!(s[3], Seg::Close);
        let s = parse_path("M1 1L2 2 3 3").unwrap();
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn line_commands() {
        let s = parse_path("M1 1H4V5h-1v-1l1 1Z").unwrap();
        assert_eq!(
            s,
            vec![
                Seg::Move((1.0, 1.0)),
                Seg::Line((4.0, 1.0)),
                Seg::Line((4.0, 5.0)),
                Seg::Line((3.0, 5.0)),
                Seg::Line((3.0, 4.0)),
                Seg::Line((4.0, 5.0)),
                Seg::Close
            ]
        );
    }

    #[test]
    fn curve_commands_and_reflection() {
        let s = parse_path("M0 0C1 0 2 1 2 2S3 4 4 4 q1 0 1 1t1 1").unwrap();
        // S reflects (2,1) about (2,2) -> (2,3)
        assert_eq!(s[2], Seg::Cubic((2.0, 3.0), (3.0, 4.0), (4.0, 4.0)));
        assert_eq!(s[3], Seg::Quad((5.0, 4.0), (5.0, 5.0)));
        // T reflects (5,4) about (5,5) -> (5,6)
        assert_eq!(s[4], Seg::Quad((5.0, 6.0), (6.0, 6.0)));
        // S with no previous cubic uses the current point as its first control
        let s = parse_path("M1 1S2 2 3 3").unwrap();
        assert_eq!(s[1], Seg::Cubic((1.0, 1.0), (2.0, 2.0), (3.0, 3.0)));
    }

    #[test]
    fn arcs_become_cubics_ending_on_target() {
        // Half circle of radius 1: two quarter cubics, midpoint on the circle.
        let s = parse_path("M0 0a1 1 0 0 1 2 0").unwrap();
        assert_eq!(s.len(), 3);
        let Seg::Cubic(_, _, mid) = s[1] else { panic!() };
        assert!(close(mid, (1.0, -1.0)), "{mid:?}");
        assert!(close(last(&s), (2.0, 0.0)));
        // Glued flags, as Simple Icons writes them: "a1 1 0 01-2 0".
        let s = parse_path("M2 0a1 1 0 01-2 0").unwrap();
        let Seg::Cubic(_, _, mid) = s[1] else { panic!() };
        assert!(close(mid, (1.0, 1.0)), "{mid:?}");
        // Radii too small are scaled up; zero radius is a line.
        assert!(parse_path("M0 0A.1 .1 0 0 1 2 0").unwrap().len() >= 2);
        assert_eq!(parse_path("M0 0A0 1 0 0 1 2 0").unwrap()[1], Seg::Line((2.0, 0.0)));
    }

    #[test]
    fn drawing_after_close_restarts_the_subpath() {
        let s = parse_path("M1 1h2zl1 1").unwrap();
        assert_eq!(s[3], Seg::Move((1.0, 1.0)));
        assert_eq!(s[4], Seg::Line((2.0, 2.0)));
    }

    #[test]
    fn bad_paths_are_errors() {
        for bad in ["L1 1", "M1", "M1 1 x", "M1 1z 2 2", "M1 1A1 1 0 2 0 1 1", "M1 1#"] {
            assert!(parse_path(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_path("").unwrap(), vec![]);
    }

    #[test]
    fn embedded_paths_parse_and_fit_the_box() {
        for icon in ALL {
            let d = match icon {
                Icon::Apple => APPLE,
                Icon::Windows => WINDOWS,
                Icon::Linux => LINUX,
                Icon::Android => ANDROID,
            };
            let segs = parse_path(d).unwrap_or_else(|e| panic!("{icon:?}: {e:?}"));
            assert!(!segs.is_empty());
            let mut cur = (0.0, 0.0);
            let mut pts = Vec::new();
            for seg in &segs {
                match *seg {
                    Seg::Move(p) | Seg::Line(p) => {
                        pts.push(p);
                        cur = p;
                    }
                    Seg::Quad(c, p) => {
                        for i in 1..=16 {
                            let t = i as f32 / 16.0;
                            let u = 1.0 - t;
                            pts.push((
                                u * u * cur.0 + 2.0 * u * t * c.0 + t * t * p.0,
                                u * u * cur.1 + 2.0 * u * t * c.1 + t * t * p.1,
                            ));
                        }
                        cur = p;
                    }
                    Seg::Cubic(c1, c2, p) => {
                        for i in 1..=16 {
                            let t = i as f32 / 16.0;
                            let u = 1.0 - t;
                            let w = |a: f32, b: f32, c: f32, d: f32| {
                                u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d
                            };
                            pts.push((w(cur.0, c1.0, c2.0, p.0), w(cur.1, c1.1, c2.1, p.1)));
                        }
                        cur = p;
                    }
                    Seg::Close => {}
                }
            }
            for (x, y) in pts {
                assert!(
                    (-0.01..=24.01).contains(&x) && (-0.01..=24.01).contains(&y),
                    "{icon:?} ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn icon_for_device() {
        let c = DeviceKind::Computer;
        assert_eq!(Icon::of(c, Some(Os::MacOs)), Some(Icon::Apple));
        assert_eq!(Icon::of(c, Some(Os::Windows)), Some(Icon::Windows));
        assert_eq!(Icon::of(c, Some(Os::Linux)), Some(Icon::Linux));
        assert_eq!(Icon::of(c, Some(Os::Android)), Some(Icon::Android));
        assert_eq!(Icon::of(c, None), None);
        // A remote device (an Apple TV) is Apple, whatever its placeholder OS.
        for locked in [false, true] {
            let r = DeviceKind::Remote { locked };
            assert_eq!(Icon::of(r, Some(Os::Linux)), Some(Icon::Apple));
            assert_eq!(Icon::of(r, None), Some(Icon::Apple));
        }
    }
}
