// SPDX-License-Identifier: AGPL-3.0-only
//! The brand palette (assets/brand/reference/08_color_palette.png), and nothing else.
//! Every other shade is one of these five at reduced opacity.

use iced::theme::{Mode, Palette};
use iced::widget::{button, container, text_input};
use iced::{Background, Border, Color, Shadow, Theme};

pub const INDIGO: Color = Color::from_rgb8(0x6A, 0x5C, 0xFF); // Electric Indigo: actions, focus
pub const VIOLET: Color = Color::from_rgb8(0x7A, 0x3F, 0xF2); // Violet Blue: the brand ribbon
pub const TEAL: Color = Color::from_rgb8(0x14, 0xB8, 0xA6); // Teal: live, "the cursor is here"
pub const INK: Color = Color::from_rgb8(0x0B, 0x0B, 0x10); // Near-Black
pub const PAPER: Color = Color::from_rgb8(0xF8, 0xF9, 0xFB); // Near-White

pub fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

pub fn theme(mode: Mode) -> Theme {
    let dark = mode == Mode::Dark;
    Theme::custom(
        if dark { "Overdesk Dark" } else { "Overdesk Light" },
        Palette {
            background: if dark { INK } else { PAPER },
            text: if dark { PAPER } else { INK },
            primary: INDIGO,
            success: TEAL,
            warning: VIOLET,
            danger: VIOLET,
        },
    )
}

/// The text colour of the current theme (Near-Black or Near-White).
pub fn ink(theme: &Theme) -> Color {
    theme.palette().text
}

pub fn muted(theme: &Theme) -> Color {
    alpha(ink(theme), 0.58)
}

pub fn hairline(theme: &Theme) -> Color {
    alpha(ink(theme), 0.1)
}

fn radius(r: f32) -> Border {
    Border {
        radius: r.into(),
        ..Border::default()
    }
}

pub fn primary(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed => alpha(INDIGO, 0.88),
        button::Status::Disabled => alpha(INDIGO, 0.35),
        button::Status::Active => INDIGO,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: PAPER,
        border: radius(10.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn secondary(theme: &Theme, status: button::Status) -> button::Style {
    let fill = match status {
        button::Status::Hovered | button::Status::Pressed => 0.08,
        _ => 0.0,
    };
    button::Style {
        background: Some(Background::Color(alpha(ink(theme), fill))),
        text_color: ink(theme),
        border: Border {
            color: alpha(ink(theme), 0.16),
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

/// One half of the Left / Right choice.
pub fn segment(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut s = secondary(theme, status);
        if selected {
            s.background = Some(Background::Color(alpha(INDIGO, 0.14)));
            s.border.color = INDIGO;
            s.text_color = ink(theme);
        }
        s
    }
}

pub fn field(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    text_input::Style {
        background: Background::Color(alpha(ink(theme), 0.04)),
        border: Border {
            color: if focused { INDIGO } else { alpha(ink(theme), 0.16) },
            width: if focused { 2.0 } else { 1.0 },
            radius: 10.0.into(),
        },
        icon: muted(theme),
        placeholder: alpha(ink(theme), 0.38),
        value: ink(theme),
        selection: alpha(INDIGO, 0.3),
    }
}

/// The quiet surface the Desk map and forms sit on.
pub fn panel(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(alpha(ink(theme), 0.03))),
        border: Border {
            color: hairline(theme),
            width: 1.0,
            radius: 14.0.into(),
        },
        ..container::Style::default()
    }
}
