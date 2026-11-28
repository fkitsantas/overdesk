// SPDX-License-Identifier: AGPL-3.0-only
//! What the app draws: the screens, their rows and buttons, and the banners above them.

use std::net::SocketAddr;
use std::sync::OnceLock;

use iced::theme::Mode as ThemeMode;
use iced::widget::{
    Space, button, canvas, column, container, image, row, scrollable, stack, text, text_input, toggler,
};
use iced::{Alignment, Element, Fill, Font, Length, Theme, font};
use overdesk_proto::{Os, PeerId};
use overdeskd::controller::{AppState, Command, DeviceKind, FileOffer, NearbyDevice, PeerView, Phase, Placement};
use overdeskd::{Mode, Side};

use super::{App, EDITION, Edition, INTER, Message, Permission, Screen, backdrop, desk, style};

/// Digits in the PIN a remote device shows.
pub(super) const PIN_DIGITS: usize = 4;

/// The wordmark lockup for light backdrops, cut from `04_light_lockup.png` with its card
/// background removed (`assets/brand/lockup-light.png`).
const LOCKUP_LIGHT: &[u8] = include_bytes!("../../../assets/brand/lockup-light.png");
/// The wordmark lockup for dark backdrops, cut from `05_dark_lockup.png` with its card
/// background removed (`assets/brand/lockup-dark.png`).
const LOCKUP_DARK: &[u8] = include_bytes!("../../../assets/brand/lockup-dark.png");
/// Decoded light lockup, shared across frames.
static LOCKUP_LIGHT_HANDLE: OnceLock<image::Handle> = OnceLock::new();
/// Decoded dark lockup, shared across frames.
static LOCKUP_DARK_HANDLE: OnceLock<image::Handle> = OnceLock::new();
/// How far content is pushed down to clear the macOS traffic lights in the merged title bar.
const TITLE_BAR: f32 = 30.0;

impl App {
    /// The backdrop, with the current screen on top: the logo as hero, or on the connected
    /// screen a compact header above the Desk map.
    pub(super) fn view(&self) -> Element<'_, Message> {
        let body: Element<'_, Message> = match &self.screen {
            Screen::Permissions {
                input_monitoring,
                accessibility,
            } => permissions_view(*input_monitoring, *accessibility),
            Screen::Starting => text("Starting…").style(muted).into(),
            Screen::Failed(e) => column![
                title("Overdesk couldn't start"),
                text(e.as_str()).style(muted).align_x(Alignment::Center),
                Space::new().height(12),
                row![
                    big("Try again").style(style::primary).on_press(Message::Retry),
                    big("Quit").style(style::secondary).on_press(Message::Quit),
                ]
                .spacing(10),
            ]
            .spacing(10)
            .align_x(Alignment::Center)
            .into(),
            Screen::Running { state, .. } => {
                let view = self.running_view(state);
                let view = match &state.offer {
                    Some(o) => column![offer_banner(o), Space::new().height(12), view].into(),
                    None => view,
                };
                match &state.notice {
                    Some(n) => column![
                        banner(n, Message::Send(Command::DismissNotice)),
                        Space::new().height(12),
                        view
                    ]
                    .into(),
                    None => view,
                }
            }
        };
        let body: Element<'_, Message> = match &self.notice {
            Some(n) => column![banner(n, Message::DismissNotice), Space::new().height(12), body].into(),
            None => body,
        };
        let connected = matches!(
            &self.screen,
            Screen::Running {
                state: AppState {
                    phase: Phase::Connected { .. },
                    ..
                },
                ..
            }
        );
        // The brand lockup crowns every screen, at its native size so it reads as the logo
        // it is: the light lockup on light backdrops, the dark lockup on dark ones.
        let (lockup, (w, h)) = lockup(self.theme);
        let top: Element<'_, Message> = container(image(lockup).width(w).height(h)).center_x(Fill).into();
        let page = column![top, Space::new().height(if connected { 20 } else { 22 }), body]
            .max_width(560)
            .align_x(Alignment::Center);
        stack![
            canvas(backdrop::Backdrop).width(Fill).height(Fill),
            container(page)
                .padding(iced::Padding {
                    top: TITLE_BAR + 6.0,
                    right: 32.0,
                    bottom: 24.0,
                    left: 32.0
                })
                .center_x(Fill)
                .height(Fill)
                .align_y(if connected {
                    iced::alignment::Vertical::Top
                } else {
                    iced::alignment::Vertical::Center
                })
        ]
        .into()
    }

    /// The screen for each controller phase.
    fn running_view<'a>(&'a self, state: &'a AppState) -> Element<'a, Message> {
        let me = &state.name;
        match &state.phase {
            Phase::Idle => self.idle_view(state),
            Phase::Connecting { address } => column![
                title(format!("Connecting to {}…", address.ip())),
                subtitle("Waiting for the other computer to answer."),
                Space::new().height(18),
                cancel(big("Cancel")),
            ]
            .spacing(8)
            .align_x(Alignment::Center)
            .into(),
            Phase::Pairing { peer, code } => self.pairing_view(peer, *code),
            Phase::EnterPin { name } => self.pin_view(name),
            Phase::Connected { peers, mode } => self.connected_view(state, peers, *mode),
            Phase::Lost { peer } => column![
                title(format!("Lost connection to {peer}")),
                subtitle(format!("Your keyboard and mouse are back on {me}.")),
                Space::new().height(18),
                cancel(big("Done")),
            ]
            .spacing(8)
            .align_x(Alignment::Center)
            .into(),
            Phase::Error(message) => column![
                title("That didn't work"),
                subtitle(message.as_str()),
                Space::new().height(18),
                big("Try again")
                    .style(style::primary)
                    .on_press(Message::Send(Command::Disconnect)),
            ]
            .spacing(8)
            .align_x(Alignment::Center)
            .into(),
        }
    }

    /// The pairing code both screens must show, and the user's answer to it.
    fn pairing_view<'a>(&self, peer: &str, code: u32) -> Element<'a, Message> {
        let answer = |matches| Message::Send(Command::Answer { code, matches });
        column![
            title("Check the code"),
            subtitle(format!("Make sure {peer} shows the same code before you continue.")),
            Space::new().height(18),
            text(format!("{:03} {:03}", code / 1000, code % 1000))
                .size(56)
                .font(bold()),
            Space::new().height(18),
            if self.answered {
                // Cancel stays: the other person may never answer.
                Element::from(
                    column![
                        subtitle(format!("Waiting for {peer} to confirm…")),
                        cancel(wide("Cancel")),
                    ]
                    .spacing(10)
                    .align_x(Alignment::Center),
                )
            } else {
                row![
                    wide("Codes match").style(style::primary).on_press(answer(true)),
                    wide("Cancel").style(style::secondary).on_press(answer(false)),
                ]
                .spacing(10)
                .into()
            },
        ]
        .spacing(6)
        .align_x(Alignment::Center)
        .into()
    }

    /// The PIN field for a remote device that shows a PIN.
    fn pin_view<'a>(&'a self, name: &str) -> Element<'a, Message> {
        let full = self.pin.len() == PIN_DIGITS;
        let submit = Message::Send(Command::SubmitPin(self.pin.clone()));
        let go = wide("Pair").style(style::primary);
        column![
            title("Enter the PIN"),
            subtitle(format!("{name} is showing a {PIN_DIGITS}-digit PIN on its screen.")),
            Space::new().height(18),
            {
                let field = text_input("PIN", &self.pin)
                    .on_input(Message::Pin)
                    .padding(12)
                    .size(28)
                    .width(160)
                    .style(style::field);
                if full { field.on_submit(submit.clone()) } else { field }
            },
            Space::new().height(12),
            row![if full { go.on_press(submit) } else { go }, cancel(wide("Cancel"))].spacing(10),
        ]
        .spacing(6)
        .align_x(Alignment::Center)
        .into()
    }

    /// The Desk map with its status line, the arrange rows (Pro) and Disconnect.
    fn connected_view<'a>(&'a self, state: &'a AppState, peers: &'a [PeerView], mode: Mode) -> Element<'a, Message> {
        let me = &state.name;
        let first = peers.first();
        let peer = first.map_or("", |p| p.name.as_str());
        let peer_word = first.map_or("Windows", |p| match (&p.label, p.kind) {
            (Some(label), _) => label.as_str(),
            (None, DeviceKind::Remote { .. }) => "Remote device",
            (None, DeviceKind::Computer) => os_word(p.os),
        });
        let status = match mode {
            Mode::Local => format!("Cursor is on {me}"),
            Mode::Driving(_) => format!("Cursor is on {peer}"),
            Mode::Driven(_) => format!("{peer} is using {me}"),
        };
        let hint = match mode {
            Mode::Driving(_) => format!(
                "Press {} to take back control at any time.",
                overdeskd::escape_chord(state.os)
            ),
            Mode::Driven(_) => format!("{peer} is controlling {me}."),
            Mode::Local => format!("Move the cursor across the screen edge to use {peer}."),
        };
        column![
            container(
                canvas(desk::DeskMap {
                    screens: &state.desk,
                    focus: state.focus,
                    mode,
                    phase: self.phase,
                    can_arrange: state.can_arrange
                })
                .width(Fill)
                .height(if state.can_arrange { 170 } else { 250 }) // leaves room for the arrange rows
            )
            .padding(6)
            .style(style::panel),
            Space::new().height(16),
            text(status).size(17).font(bold()),
            text(hint).style(muted),
            Space::new().height(12),
            arrange_rows(state, peers),
            Space::new().height(6),
            row![
                column![
                    text(peer).font(bold()),
                    row![
                        text("●").size(11).style(|t: &Theme| text::Style {
                            color: Some(style::teal_text(t))
                        }),
                        text(format!("{peer_word}, connected")).style(muted)
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                ]
                .spacing(2),
                Space::new().width(Fill),
                cancel(big("Disconnect")),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(4)
        .into()
    }

    /// "Pair a device": which side the other computer is on, the computers found on the
    /// network, and a collapsed fallback for typing an address.
    fn idle_view<'a>(&'a self, state: &'a AppState) -> Element<'a, Message> {
        let me = &state.name;
        let side = |s: Side, label: &'static str| {
            // The selected one also carries a check mark, so colour is not the only cue.
            button(text(if self.form.side == s {
                format!("\u{2713} {label}")
            } else {
                label.to_string()
            }))
            .padding([8, 20])
            .style(style::segment(self.form.side == s))
            .on_press(Message::Side(s))
        };
        let mut page = column![
            title("Pair a device"),
            subtitle(format!("Share {me}'s keyboard and mouse with another computer.")),
            if state.pairing_open {
                // Pairing is only open while this window is focused: say so, so a code prompt is no surprise.
                Element::from(text("Ready to pair").size(13).style(|t: &Theme| text::Style {
                    color: Some(style::teal_text(t)),
                }))
            } else {
                Space::new().height(16).into() // the line's height, so the page does not jump
            },
            Space::new().height(10),
            text(format!("Where is it, next to {me}?")).size(13).style(muted),
            row![side(Side::Left, "On the left"), side(Side::Right, "On the right")].spacing(8),
            Space::new().height(6),
            // Two rows fit without scrolling; more scroll inside a fixed band so the
            // fallback button never falls off the bottom of the window.
            column![
                text("Nearby devices").font(bold()),
                container(scrollable(self.nearby_view(state)).spacing(4)).max_height(NEARBY_MAX_HEIGHT)
            ]
            .spacing(8)
            .width(Fill),
            self.paired_view(state),
            self.address_view(state),
        ]
        .spacing(6)
        .align_x(Alignment::Center);
        // A quiet line, only while the build's extension publishes one.
        if let Some(line) = &self.status {
            let mut status = row![text(line.text.as_str()).size(13).style(muted)]
                .spacing(10)
                .align_y(Alignment::Center);
            if let Some((label, enabled)) = &line.button {
                status = status.push(
                    small(label.as_str())
                        .style(style::secondary)
                        .on_press_maybe((*enabled).then_some(Message::ExtensionButton)),
                );
            }
            page = page.push(Space::new().height(8)).push(status);
        }
        // Quiet and last: a setting, not part of pairing.
        let mut settings = row![
            toggler(self.login)
                .label("Open at login")
                .text_size(13)
                .size(16)
                .on_toggle(|_| Message::ToggleLogin)
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if let Some(e) = &self.login_error {
            settings = settings.push(text(e.as_str()).size(13).style(muted));
        }
        page = page.push(Space::new().height(8)).push(settings);
        if let Some(edition) = EDITION.get() {
            page = page.push(self.licence_view(edition));
        }
        page.into()
    }

    /// The computers heard on the network, one row each, or a waiting message.
    fn nearby_view<'a>(&'a self, state: &'a AppState) -> Element<'a, Message> {
        if state.nearby.is_empty() {
            return column![
                text("Looking for computers on your network…").font(bold()),
                text("Open Overdesk on your other computer. It will appear here.").style(muted),
            ]
            .spacing(4)
            .width(Fill)
            .align_x(Alignment::Center)
            .into();
        }
        column(state.nearby.iter().map(|d| self.nearby_row(d)))
            .spacing(10)
            .into()
    }

    /// One nearby device: its glyph, name, state, and Connect or Pair (and Forget when paired).
    fn nearby_row<'a>(&'a self, d: &'a NearbyDevice) -> Element<'a, Message> {
        let remote = matches!(d.kind, DeviceKind::Remote { .. });
        let locked = matches!(d.kind, DeviceKind::Remote { locked: true });
        let confirming = self.confirm_forget == Some(d.id);
        let go = button(text(if d.paired { "Connect" } else { "Pair" }))
            .padding([8, 20])
            .style(style::primary);
        // A locked remote stays visible, but its button does nothing.
        let go = if locked {
            go
        } else {
            go.on_press(Message::Send(Command::Connect {
                address: d.address,
                side: self.form.side,
                // Hold a paired device to its pinned identity; a first pairing has none. A
                // remote device is always named: the controller pairs it if it must.
                expect: (d.paired || remote).then_some(d.id),
            }))
        };
        // A paired device can be forgotten: unpaired and taken off the Desk.
        let forget: Element<'a, Message> = if d.paired && !locked {
            forget_control(&d.name, d.id, confirming)
        } else {
            Space::new().into()
        };
        let mut info = row![
            text(if locked {
                "Needs Overdesk Pro"
            } else if remote {
                "Remote device"
            } else {
                os_word(d.os)
            })
            .style(muted)
        ]
        .spacing(8);
        if d.paired {
            info = info.push(text("Paired").style(|t: &Theme| text::Style {
                color: Some(style::teal_text(t)),
            }));
        }
        let initial = match (remote, d.os) {
            (true, _) => 'T',
            (_, Os::Windows) => 'W',
            (_, Os::MacOs) => 'M',
            _ => d.name.chars().next().map_or(' ', |c| c.to_ascii_uppercase()),
        };
        container(
            row![
                container(text(initial.to_string()).size(15).font(bold()))
                    .center(38)
                    .style(style::glyph),
                column![text(d.name.as_str()).font(bold()), info].spacing(2),
                Space::new().width(Fill),
                forget,
                // Asking first: the row offers only the answer to the question.
                if confirming {
                    Space::new().into()
                } else {
                    Element::from(go)
                },
            ]
            .spacing(14)
            .align_y(Alignment::Center),
        )
        .padding([12, 16])
        .width(Fill)
        .style(style::panel)
        .into()
    }

    /// The collapsed "Enter an address instead" fallback and, when open, the address field.
    fn address_view<'a>(&'a self, state: &AppState) -> Element<'a, Message> {
        let connect = button(text("Connect")).padding([9, 22]).style(style::primary);
        let connect = if self.form.target().is_some() {
            connect.on_press(Message::Connect)
        } else {
            connect
        };
        let mut fallback = column![
            small(if self.form.open {
                "Hide address"
            } else {
                "Enter an address instead"
            })
            .style(style::secondary)
            .on_press(Message::ToggleAddress)
        ]
        .spacing(10)
        .align_x(Alignment::Center);
        if self.form.open {
            if let Some(line) = address_line(state.address, self.form.lan_ip) {
                fallback = fallback.push(text(line).size(13).style(muted));
            }
            fallback = fallback.push(
                row![
                    text_input("192.168.1.50", &self.form.address)
                        .on_input(Message::Address)
                        .on_submit(Message::Connect)
                        .padding(10)
                        .style(style::field),
                    connect
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        }
        fallback.into()
    }
}

impl App {
    /// "Paired devices": the paired ones that are neither nearby nor connected, collapsed by
    /// default, each with a Forget that asks first. Empty when there are none.
    fn paired_view<'a>(&'a self, state: &'a AppState) -> Element<'a, Message> {
        let connected = |id| matches!(&state.phase, Phase::Connected { peers, .. } if peers.iter().any(|p| p.id == id));
        let away: Vec<_> = state.paired.iter().filter(|d| !d.online && !connected(d.id)).collect();
        if away.is_empty() {
            return Space::new().into();
        }
        let toggle = small(format!(
            "{} paired devices ({})",
            if self.show_paired { "Hide" } else { "Show" },
            away.len()
        ))
        .style(style::secondary)
        .on_press(Message::TogglePaired);
        let mut section = column![toggle].spacing(8).align_x(Alignment::Center).width(Fill);
        if self.show_paired {
            for d in away {
                let action = forget_control(&d.name, d.id, self.confirm_forget == Some(d.id));
                section = section.push(
                    container(
                        row![text(d.name.as_str()).font(bold()), Space::new().width(Fill), action]
                            .spacing(14)
                            .align_y(Alignment::Center),
                    )
                    .padding([10, 16])
                    .width(Fill)
                    .style(style::panel),
                );
            }
        }
        section.into()
    }

    /// The "Overdesk Pro" row: the licensee, a restart prompt after activating, or a key field.
    fn licence_view<'a>(&'a self, edition: &'a Edition) -> Element<'a, Message> {
        if let Some(email) = &edition.licensed_to {
            return text(format!("Pro, licensed to {email}")).size(13).style(muted).into();
        }
        if let Some(email) = &self.licence.activated {
            return row![
                text(format!("Pro, licensed to {email}")).size(13).style(muted),
                small("Restart to finish")
                    .style(style::primary)
                    .on_press(Message::Relaunch),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into();
        }
        let go = small("Activate").style(style::secondary);
        let go = if self.licence.key.trim().is_empty() {
            go
        } else {
            go.on_press(Message::Activate)
        };
        let mut licence = column![
            row![
                text_input("Licence key", &self.licence.key)
                    .on_input(Message::Key)
                    .on_submit(Message::Activate)
                    .size(13)
                    .padding(8)
                    .style(style::field),
                go
            ]
            .spacing(8)
            .align_y(Alignment::Center)
        ]
        .spacing(4);
        if let Some(e) = &self.licence.error {
            licence = licence.push(text(e.as_str()).size(13).style(muted));
        }
        licence.into()
    }
}

/// An operating system, named for people. A secondary detail: devices are named by their
/// own computer name, never by a guessed role.
fn os_word(os: Os) -> &'static str {
    match os {
        Os::MacOs => "macOS",
        Os::Windows => "Windows",
        Os::Linux => "Linux",
        Os::Android => "Android TV",
    }
}

/// A screen title: large and semibold.
fn title<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).size(29).font(bold()).align_x(Alignment::Center)
}

/// A button with its label at the page's main size; the caller adds style and action.
fn big<'a>(label: impl text::IntoFragment<'a>) -> iced::widget::Button<'a, Message> {
    button(text(label)).padding([10, 22])
}

/// [`big`] with the wider padding the pairing screens use.
fn wide<'a>(label: impl text::IntoFragment<'a>) -> iced::widget::Button<'a, Message> {
    button(text(label)).padding([11, 26])
}

/// A button for a side action: small label, the style and action left to the caller.
fn small<'a>(label: impl text::IntoFragment<'a>) -> iced::widget::Button<'a, Message> {
    button(text(label).size(13)).padding([6, 12])
}

/// The smallest button, for rows of options and banners.
fn tiny<'a>(label: impl text::IntoFragment<'a>) -> iced::widget::Button<'a, Message> {
    button(text(label).size(13)).padding([4, 10])
}

/// Makes `b` the secondary button that ends the current attempt or session.
fn cancel(b: iced::widget::Button<'_, Message>) -> iced::widget::Button<'_, Message> {
    b.style(style::secondary).on_press(Message::Send(Command::Disconnect))
}

/// The "Place" and Disconnect rows for each peer, shown only when the Desk can be arranged.
/// Buttons, and dragging a screen on the map, both send `Place`; the buttons also work
/// without a pointer.
fn arrange_rows<'a>(state: &'a AppState, peers: &'a [PeerView]) -> Element<'a, Message> {
    let anchor = state.desk.iter().find(|s| s.me).map(|s| s.id);
    column(
        peers
            .iter()
            .filter(|_| state.can_arrange)
            .filter_map(|p| anchor.map(|a| (p, a)))
            .map(|(p, a)| {
                let at = |label: &'static str, side| {
                    tiny(label)
                        .style(style::secondary)
                        .on_press(Message::Send(Command::Place {
                            peer: p.id,
                            anchor: a,
                            side,
                        }))
                };
                row![
                    text(format!("Place {}", p.name)).size(13).style(muted),
                    Space::new().width(Fill),
                    at("Left", Placement::Left),
                    at("Right", Placement::Right),
                    at("Above", Placement::Above),
                    at("Below", Placement::Below),
                ]
                .extend((peers.len() > 1).then(|| {
                    tiny("Disconnect")
                        .style(style::secondary)
                        .on_press(Message::Send(Command::DisconnectPeer(p.id)))
                        .into()
                }))
                .spacing(6)
                .align_y(Alignment::Center)
                .into()
            }),
    )
    .spacing(4)
    .into()
}

/// Muted supporting text under a title, centred.
fn subtitle<'a>(s: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).style(muted).align_x(Alignment::Center)
}

/// The tallest the Nearby list grows before it scrolls: two rows and their spacing.
const NEARBY_MAX_HEIGHT: f32 = 156.0;

/// The lockup for the current theme and its native size in pixels.
fn lockup(theme: ThemeMode) -> (image::Handle, (u32, u32)) {
    if theme == ThemeMode::Dark {
        (
            LOCKUP_DARK_HANDLE
                .get_or_init(|| image::Handle::from_bytes(LOCKUP_DARK))
                .clone(),
            (424, 60),
        )
    } else {
        (
            LOCKUP_LIGHT_HANDLE
                .get_or_init(|| image::Handle::from_bytes(LOCKUP_LIGHT))
                .clone(),
            (430, 58),
        )
    }
}

/// Inter Semibold, for titles and emphasis.
fn bold() -> Font {
    Font {
        weight: font::Weight::Semibold,
        ..INTER
    }
}

/// Secondary text in the theme's muted ink.
fn muted(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(style::muted(theme)),
    }
}

/// The macOS permissions screen: one row per permission, then a button to reopen.
fn permissions_view<'a>(input_monitoring: bool, accessibility: bool) -> Element<'a, Message> {
    let item = |granted: bool, name: &'a str, why: &'a str, which: Permission| {
        let state: Element<'a, Message> = if granted {
            text("Granted").color(style::TEAL).into()
        } else {
            button(text("Open Settings"))
                .padding([8, 14])
                .style(style::secondary)
                .on_press(Message::OpenSettings(which))
                .into()
        };
        container(
            row![
                column![text(name).font(bold()), text(why).style(muted)]
                    .spacing(2)
                    .width(Fill),
                state
            ]
            .spacing(12)
            .align_y(Alignment::Center),
        )
        .padding([12, 16])
        .style(style::panel)
    };
    column![
        title("Overdesk needs two permissions"),
        subtitle("macOS asks you to approve these once. Overdesk never records or stores what you type."),
        Space::new().height(8),
        item(
            input_monitoring,
            "Input Monitoring",
            "Lets Overdesk see your keyboard and mouse so they can drive your other computers.",
            Permission::InputMonitoring
        ),
        item(
            accessibility,
            "Accessibility",
            "Lets your other computers type and click on this Mac.",
            Permission::Accessibility
        ),
        Space::new().height(8),
        row![
            text("After granting both, reopen Overdesk.").style(muted),
            Space::new().width(Fill),
            big("Reopen Overdesk").style(style::primary).on_press(Message::Relaunch),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(10)
    .width(Length::Fill)
    .align_x(Alignment::Center)
    .into()
}

/// A dismissible notice strip above the current screen.
fn banner(text_: &str, dismiss: Message) -> Element<'_, Message> {
    container(
        row![
            text(text_).size(13).style(muted).width(Fill),
            tiny("Dismiss").style(style::secondary).on_press(dismiss),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding(10)
    .style(style::panel)
    .into()
}

/// "12 MB", "3.4 KB": a size for people, in decimal units.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let (mut n, mut unit) = (bytes as f64, 0);
    while n >= 1000.0 && unit < UNITS.len() - 1 {
        n /= 1000.0;
        unit += 1;
    }
    if unit == 0 || n >= 10.0 {
        format!("{n:.0} {}", UNITS[unit])
    } else {
        format!("{n:.1} {}", UNITS[unit])
    }
}

/// The question "Accept 3 files (12 MB) from MacBook?" with its two answers.
fn offer_text(o: &FileOffer) -> String {
    let files = if o.count == 1 { "file" } else { "files" };
    format!(
        "Accept {} {files} ({}) from {}?",
        o.count,
        human_bytes(o.bytes),
        o.from_name
    )
}

/// A strip asking whether to receive the files another computer offers.
fn offer_banner(o: &FileOffer) -> Element<'_, Message> {
    // The answer names the offer the banner shows, so a click on an older banner answers nothing.
    let answer = |accept| Message::Send(Command::AcceptFiles { id: o.id, accept });
    container(
        row![
            text(offer_text(o)).size(13).style(muted).width(Fill),
            tiny("Decline").style(style::secondary).on_press(answer(false)),
            tiny("Accept").on_press(answer(true)),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding(10)
    .style(style::panel)
    .into()
}

/// A Forget button, or once `confirming` the question with its Forget and Cancel: one flow for
/// the nearby list and the paired list.
fn forget_control<'a>(name: &str, id: PeerId, confirming: bool) -> Element<'a, Message> {
    if confirming {
        row![
            text(forget_prompt(name)).size(13),
            small("Forget").style(style::primary).on_press(Message::ConfirmForget),
            small("Cancel").style(style::secondary).on_press(Message::CancelForget),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into()
    } else {
        small("Forget")
            .style(style::secondary)
            .on_press(Message::AskForget(id))
            .into()
    }
}

/// What to tell the user to type on the other computer: this computer's address with the port
/// it listens on. `bound` is the listening socket, often the unspecified address; `lan` fills it in.
fn address_line(bound: Option<SocketAddr>, lan: Option<std::net::IpAddr>) -> Option<String> {
    let bound = bound?;
    let ip = if bound.ip().is_unspecified() { lan? } else { bound.ip() };
    Some(format!("This computer: {}", SocketAddr::new(ip, bound.port())))
}

/// The question asked before a paired device is forgotten.
fn forget_prompt(name: &str) -> String {
    format!("Forget {name}? You'll need to pair again.")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer id made of one repeated byte.
    fn id_of(n: u8) -> PeerId {
        PeerId([n; 16])
    }

    #[test]
    fn the_address_to_type_uses_the_lan_ip_when_listening_everywhere() {
        let lan = Some("192.168.1.5".parse().unwrap());
        let any = Some(SocketAddr::from(([0, 0, 0, 0], 24801)));
        assert_eq!(
            address_line(any, lan).as_deref(),
            Some("This computer: 192.168.1.5:24801")
        );
        assert_eq!(address_line(any, None), None, "no network, nothing to show");
        let bound = Some(SocketAddr::from(([10, 0, 0, 2], 7)));
        assert_eq!(address_line(bound, lan).as_deref(), Some("This computer: 10.0.0.2:7"));
        assert_eq!(address_line(None, lan), None);
    }

    #[test]
    fn the_file_offer_reads_as_a_question() {
        let o = |count, bytes| FileOffer {
            from: id_of(1),
            id: [7; 16],
            from_name: "MacBook".into(),
            count,
            bytes,
        };
        assert_eq!(offer_text(&o(3, 12_000_000)), "Accept 3 files (12 MB) from MacBook?");
        assert_eq!(offer_text(&o(1, 2_500)), "Accept 1 file (2.5 KB) from MacBook?");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(0), "0 B");
    }

    #[test]
    fn forgetting_asks_by_name() {
        assert_eq!(forget_prompt("MacBook"), "Forget MacBook? You'll need to pair again.");
    }
}
