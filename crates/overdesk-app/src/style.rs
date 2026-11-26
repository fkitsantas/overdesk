// SPDX-License-Identifier: AGPL-3.0-only
//! The brand palette (assets/brand/reference/08_color_palette.png), and nothing else.
//! Every other shade is one of these five at reduced opacity.

use iced::gradient::Linear;
use iced::theme::{Mode, Palette};
use iced::widget::{button, container, text_input};
use iced::{Background, Border, Color, Radians, Shadow, Theme, Vector};

/// Electric Indigo, from the palette board: buttons, focus and selection.
pub const INDIGO: Color = Color::from_rgb8(0x6A, 0x5C, 0xFF); // Electric Indigo: actions, focus
/// Violet Blue, from the palette board: the start of the brand ribbon.
pub const VIOLET: Color = Color::from_rgb8(0x7A, 0x3F, 0xF2); // Violet Blue: the brand ribbon
/// Teal, from the palette board: the screen that has the cursor, and "connected".
pub const TEAL: Color = Color::from_rgb8(0x14, 0xB8, 0xA6); // Teal: live, "the cursor is here"
/// Near-Black, from the palette board: text in light mode, the background in dark.
pub const INK: Color = Color::from_rgb8(0x0B, 0x0B, 0x10); // Near-Black
/// Near-White, from the palette board: the background in light mode, text in dark.
pub const PAPER: Color = Color::from_rgb8(0xF8, 0xF9, 0xFB); // Near-White

/// `c` at opacity `a`: how every shade beyond the five brand colours is made.
pub fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

/// The iced theme for light or dark mode, built from the brand palette alone.
pub fn theme(mode: Mode) -> Theme {
    let dark = mode == Mode::Dark;
    Theme::custom(
        if dark { "Overdesk Dark" } else { "Overdesk Light" },
        Palette {
            background: if dark { INK } else { PAPER },
            text: if dark { PAPER } else { INK },
            primary: INDIGO,
            success: TEAL,
            // The palette has no amber or red, so warnings and errors use Violet Blue.
            warning: VIOLET,
            danger: VIOLET,
        },
    )
}

/// The text colour of the current theme (Near-Black or Near-White).
pub fn ink(theme: &Theme) -> Color {
    theme.palette().text
}

/// Secondary text: the theme's ink at reduced opacity.
pub fn muted(theme: &Theme) -> Color {
    alpha(ink(theme), 0.58)
}

/// Dividers and resting borders: the theme's ink, barely there.
pub fn hairline(theme: &Theme) -> Color {
    alpha(ink(theme), 0.1)
}

/// Whether the theme is the dark one.
pub fn is_dark(theme: &Theme) -> bool {
    theme.palette().background == INK
}

/// A radius that always rounds a button's ends fully, whatever its height.
const PILL: f32 = 999.0;

/// A glass fill: `PAPER` over dark surfaces, `INK` over light ones, both nearly transparent.
fn glass(theme: &Theme, dark_alpha: f32, light_alpha: f32) -> Color {
    if is_dark(theme) {
        alpha(PAPER, dark_alpha)
    } else {
        alpha(INK, light_alpha)
    }
}

/// The filled gradient pill (Violet to Indigo) for the main action on a screen.
pub fn primary(_theme: &Theme, status: button::Status) -> button::Style {
    let a = match status {
        button::Status::Hovered | button::Status::Pressed => 1.0,
        button::Status::Disabled => 0.35,
        button::Status::Active => 0.9,
    };
    let gradient = Linear::new(Radians(std::f32::consts::FRAC_PI_2))
        .add_stop(0.0, alpha(VIOLET, a))
        .add_stop(1.0, alpha(INDIGO, a));
    button::Style {
        background: Some(Background::Gradient(gradient.into())),
        text_color: if status == button::Status::Disabled {
            alpha(PAPER, 0.7)
        } else {
            PAPER
        },
        border: Border {
            radius: PILL.into(),
            ..Border::default()
        },
        shadow: Shadow {
            color: alpha(INDIGO, if status == button::Status::Disabled { 0.0 } else { 0.35 }),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 14.0,
        },
        snap: true,
    }
}

/// The translucent glass pill for every other action.
pub fn secondary(theme: &Theme, status: button::Status) -> button::Style {
    let hot = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(Background::Color(if hot {
            glass(theme, 0.12, 0.08)
        } else {
            glass(theme, 0.06, 0.04)
        })),
        text_color: ink(theme),
        border: Border {
            color: alpha(ink(theme), if hot { 0.22 } else { 0.14 }),
            width: 1.0,
            radius: PILL.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// One pill of the Left / Right choice; the selected one is Indigo glass with an Indigo rim.
pub fn segment(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut s = secondary(theme, status);
        if selected {
            s.background = Some(Background::Color(alpha(INDIGO, 0.2)));
            s.border.color = INDIGO;
        }
        s
    }
}

/// The text input: a glass fill, and an Indigo border while focused.
pub fn field(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: Background::Color(glass(theme, 0.06, 0.04)),
        border: Border {
            color: if focused { INDIGO } else { alpha(ink(theme), 0.16) },
            width: if focused { 2.0 } else { 1.0 },
            radius: 12.0.into(),
        },
        icon: muted(theme),
        placeholder: alpha(ink(theme), 0.38),
        value: ink(theme),
        selection: alpha(INDIGO, 0.3),
    }
}

/// The glass surface the Desk map, cards and forms sit on: translucent, hairline-edged and softly lifted.
pub fn panel(theme: &Theme) -> container::Style {
    let dark = is_dark(theme);
    container::Style {
        background: Some(Background::Color(glass(theme, 0.055, 0.035))),
        border: Border {
            color: hairline(theme),
            width: 1.0,
            radius: 18.0.into(),
        },
        shadow: Shadow {
            color: alpha(INK, if dark { 0.35 } else { 0.08 }),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    }
}

/// The small circle holding a device's initial: Indigo at 20%.
pub fn glyph(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(alpha(INDIGO, 0.2))),
        border: Border {
            color: alpha(INDIGO, 0.45),
            width: 1.0,
            radius: PILL.into(),
        },
        ..container::Style::default()
    }
}

/// The soft Indigo halo behind the hero logo, of the given corner radius.
pub fn logo_halo(corner: f32) -> impl Fn(&Theme) -> container::Style {
    move |theme| container::Style {
        border: Border {
            radius: corner.into(),
            ..Border::default()
        },
        shadow: Shadow {
            color: alpha(INDIGO, if is_dark(theme) { 0.55 } else { 0.4 }),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 40.0,
        },
        ..container::Style::default()
    }
}
