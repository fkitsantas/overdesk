// SPDX-License-Identifier: AGPL-3.0-only
//! The Overdesk app: one quiet window around the controller. It renders the state the
//! controller publishes and sends it commands; capture, the engine and every failsafe
//! live in Rust below it, so the UI can never lock anyone out.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
mod backdrop;
mod desk;
#[cfg(feature = "preview")]
mod preview;
mod style;
mod tray;

use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use iced::futures::SinkExt;
use iced::theme::Mode as ThemeMode;
use iced::widget::{Space, button, canvas, column, container, image, row, scrollable, stack, text, text_input};
use iced::{Alignment, Element, Fill, Font, Length, Subscription, Task, Theme, font};
use overdesk_core::engine::Mode;
use overdesk_net::session::Me;
use overdesk_proto::Os;
use overdeskd::Side;
use overdeskd::controller::{self, AppState, Command, Phase};
use tokio::sync::{mpsc, watch};

/// The port the controller listens on, and the one assumed when an address has none.
const PORT: u16 = 24801;
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
// Inter (SIL OFL 1.1, assets/fonts/Inter-LICENSE.txt): the grotesk of the wordmark,
// identical on macOS and Windows, with a real semibold.
/// The app's typeface, bundled so every platform renders the same.
const INTER: Font = Font::with_name("Inter");

/// The controller's live state, read by the subscription (which takes no captures).
static STATE: OnceLock<watch::Receiver<AppState>> = OnceLock::new();

/// Opens the window: Inter as the default font, the brand theme, and the controller's
/// state as a subscription.
fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("Overdesk")
        .subscription(App::subscription)
        .exit_on_close_request(false) // closing hides the window; the tray menu quits
        .theme(App::theme)
        .window(window_settings())
        .font(include_bytes!("../../../assets/fonts/Inter-Regular.ttf").as_slice())
        .font(include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf").as_slice())
        .default_font(INTER)
        .run()
}

/// The window: 760x560 (never below 640x480), with a transparent title bar merged into the
/// content on macOS so the backdrop runs to the top edge.
fn window_settings() -> iced::window::Settings {
    iced::window::Settings {
        size: iced::Size::new(760.0, 560.0),
        min_size: Some(iced::Size::new(640.0, 480.0)),
        #[cfg(target_os = "macos")]
        platform_specific: iced::window::settings::PlatformSpecific {
            title_hidden: true,
            titlebar_transparent: true,
            fullsize_content_view: true,
        },
        ..iced::window::Settings::default()
    }
}

/// A macOS privacy permission that Overdesk needs before it can start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Permission {
    /// Lets Overdesk capture the keyboard and mouse.
    InputMonitoring,
    /// Lets Overdesk inject input on this Mac.
    Accessibility,
}

/// Which top-level screen the window shows.
enum Screen {
    /// macOS needs Input Monitoring and Accessibility before anything else.
    Permissions {
        /// Whether Input Monitoring is granted.
        input_monitoring: bool,
        /// Whether Accessibility is granted.
        accessibility: bool,
    },
    /// The controller is starting.
    Starting,
    /// The controller could not start; the message says why.
    Failed(String),
    /// The controller is running; the UI renders its state.
    Running {
        /// Where UI commands go.
        commands: mpsc::UnboundedSender<Command>,
        /// The latest state the controller published.
        state: AppState,
    },
}

/// The iced application. It renders controller state, plus the little the form and
/// the animation need, and holds no connection state of its own.
struct App {
    /// The top-level screen being shown.
    screen: Screen,
    /// Light or dark, following the system.
    theme: ThemeMode,
    /// What the user has typed into the address field.
    address: String,
    /// The Left / Right choice for the other device's screen.
    side: Side,
    /// The ribbon animation's position, 0.0..1.0.
    phase: f32,
    /// Whether the user has answered the pairing code and is waiting for the peer.
    /// Reset whenever the phase leaves [`Phase::Pairing`].
    answered: bool,
    /// Whether the "Enter an address instead" fallback is expanded.
    show_address: bool,
    /// Whether the window has focus. Pairing opens only while it does: pairing needs
    /// both people looking at both screens anyway, and an unattended machine must
    /// never let a LAN device pop a code prompt. Starts `false`, so it fails closed.
    focused: bool,
    /// The menu-bar / system-tray icon, once created (never in a preview).
    tray: Option<tray::Tray>,
}

/// Everything that can change the app: controller events, system events and user input.
#[derive(Debug, Clone)]
enum Message {
    /// The controller started, or failed to.
    Started(Result<(mpsc::UnboundedSender<Command>, AppState), String>),
    /// The controller published a new state.
    State(AppState),
    /// The system switched between light and dark.
    Theme(ThemeMode),
    /// An animation frame while the cursor is on another device.
    Frame,
    /// Show or hide the address fallback.
    ToggleAddress,
    /// Re-open the pairing window while it is still wanted (see [`App::pairing_wanted`]).
    KeepPairingOpen,
    /// Create the tray icon. Queued from `boot` so it runs on the main thread inside the
    /// event loop, which macOS requires.
    CreateTray,
    /// The user picked an item from the tray menu.
    Tray(tray::Action),
    /// The user closed the window: hide it instead of quitting.
    CloseRequested(iced::window::Id),
    /// The window gained (`true`) or lost (`false`) focus.
    Focus(bool),
    /// The address field changed.
    Address(String),
    /// The user picked which side the other device is on.
    Side(Side),
    /// The user asked to connect to the typed address.
    Connect,
    /// Forward a command to the controller as is.
    Send(Command),
    /// Open System Settings at the pane for a permission.
    OpenSettings(Permission),
    /// Re-check the macOS permissions while the permissions screen is shown.
    PollPermissions,
    /// Restart the app so newly granted permissions take effect.
    Relaunch,
    /// The preview screenshot is ready to save.
    #[cfg(feature = "preview")]
    Shot(iced::window::Screenshot),
}

impl App {
    /// Builds the first screen: the permissions check on macOS, otherwise starting the
    /// controller. With `OVERDESK_PREVIEW` set, renders a fixed preview state instead.
    fn boot() -> (Self, Task<Message>) {
        #[cfg(feature = "preview")]
        if let Ok(which) = std::env::var("OVERDESK_PREVIEW") {
            let (state, perms) = preview::state(&which);
            let screen = match perms {
                Some((input_monitoring, accessibility)) => Screen::Permissions {
                    input_monitoring,
                    accessibility,
                },
                None => Screen::Running {
                    commands: mpsc::unbounded_channel().0,
                    state,
                },
            };
            let app = Self {
                screen,
                theme: preview::theme(),
                address: String::new(),
                side: Side::Right,
                phase: 0.3,
                answered: false,
                show_address: false,
                focused: false,
                tray: None,
            };
            return (app, preview::shoot());
        }
        let (granted_listen, granted_post) = permissions();
        let screen = if granted_listen && granted_post {
            Screen::Starting
        } else {
            Screen::Permissions {
                input_monitoring: granted_listen,
                accessibility: granted_post,
            }
        };
        let starting = matches!(screen, Screen::Starting);
        let app = Self {
            screen,
            theme: ThemeMode::Light,
            address: String::new(),
            side: Side::Right,
            phase: 0.0,
            answered: false,
            show_address: false,
            focused: false,
            tray: None,
        };
        let theme = iced::system::theme().map(Message::Theme);
        let theme = Task::batch([theme, Task::done(Message::CreateTray)]);
        (
            app,
            if starting {
                Task::batch([theme, Task::perform(start(), Message::Started)])
            } else {
                theme
            },
        )
    }

    /// Whether pairing should be open: only while the window is focused and the
    /// controller is idle. Pairing needs both people looking at both screens anyway.
    fn pairing_wanted(&self) -> bool {
        self.focused && matches!(&self.screen, Screen::Running { state, .. } if state.phase == Phase::Idle)
    }

    /// Applies a message, then opens or closes pairing if [`Self::pairing_wanted`] changed.
    /// Connection changes happen only by sending controller commands.
    fn update(&mut self, message: Message) -> Task<Message> {
        let before = self.pairing_wanted();
        let task = self.apply(message);
        let after = self.pairing_wanted();
        if before != after {
            self.send(Command::AllowPairing(after));
        }
        task
    }

    /// Applies a message to the app's state.
    fn apply(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Started(Ok((commands, state))) => {
                self.screen = Screen::Running { commands, state };
            }
            Message::Started(Err(e)) => self.screen = Screen::Failed(e),
            Message::State(new) => {
                if !matches!(new.phase, Phase::Pairing { .. }) {
                    self.answered = false;
                }
                if let Screen::Running { state, .. } = &mut self.screen {
                    *state = new;
                    if let Some(tray) = &self.tray {
                        tray.sync(state);
                    }
                }
            }
            Message::PollPermissions => {
                if let Screen::Permissions {
                    input_monitoring,
                    accessibility,
                } = &mut self.screen
                {
                    (*input_monitoring, *accessibility) = permissions();
                    if *input_monitoring && *accessibility {
                        // Both granted: start the controller as `boot` does, no relaunch needed.
                        self.screen = Screen::Starting;
                        return Task::perform(start(), Message::Started);
                    }
                }
            }
            Message::Theme(mode) => self.theme = mode,
            Message::Frame => self.phase = (self.phase + 0.04) % 1.0,
            Message::ToggleAddress => self.show_address = !self.show_address,
            Message::Focus(f) => self.focused = f,
            Message::CreateTray => {
                let state = match &self.screen {
                    Screen::Running { state, .. } => Some(state),
                    _ => None,
                };
                match tray::Tray::new(state) {
                    Ok(tray) => self.tray = Some(tray),
                    // Without a tray the window still works; closing it simply quits no more.
                    Err(e) => eprintln!("overdesk: tray icon unavailable: {e}"),
                }
            }
            Message::CloseRequested(id) => {
                // With no tray there would be no way back, so closing must quit then.
                return if self.tray.is_some() {
                    iced::window::set_mode(id, iced::window::Mode::Hidden)
                } else {
                    iced::exit()
                };
            }
            Message::Tray(tray::Action::Show) => {
                return iced::window::oldest().and_then(|id| {
                    Task::batch([
                        iced::window::set_mode(id, iced::window::Mode::Windowed),
                        iced::window::gain_focus(id),
                    ])
                });
            }
            Message::Tray(tray::Action::Disconnect) => self.send(Command::Disconnect),
            Message::Tray(tray::Action::Quit) => {
                self.send(Command::Disconnect);
                // Give the controller a moment to release input before the process ends.
                return Task::perform(tokio::time::sleep(std::time::Duration::from_millis(250)), |()| ())
                    .discard()
                    .chain(iced::exit());
            }
            // The subscription only exists while wanted, but a queued tick may outlive it.
            Message::KeepPairingOpen => {
                if self.pairing_wanted() {
                    self.send(Command::AllowPairing(true));
                }
            }
            Message::Address(a) => self.address = a,
            Message::Side(s) => self.side = s,
            Message::Connect => {
                if let Some(address) = parse_address(&self.address) {
                    self.send(Command::Connect {
                        address,
                        side: self.side,
                        expect: None, // a typed address has no earlier identity to hold it to
                    });
                }
            }
            Message::Send(c) => {
                self.answered |= matches!(c, Command::AnswerCode(_));
                self.send(c);
            }
            Message::OpenSettings(p) => open_settings(p),
            Message::Relaunch => relaunch(),
            #[cfg(feature = "preview")]
            Message::Shot(shot) => preview::save(shot),
        }
        Task::none()
    }

    /// Sends a command to the controller, if it is running.
    fn send(&self, c: Command) {
        if let Screen::Running { commands, .. } = &self.screen {
            let _ = commands.send(c);
        }
    }

    /// Follows the system theme and the controller's state, plus animation frames while
    /// the cursor is on another device.
    fn subscription(&self) -> Subscription<Message> {
        // A preview renders one frozen state; live updates would overwrite it.
        if cfg!(feature = "preview") && std::env::var_os("OVERDESK_PREVIEW").is_some() {
            return Subscription::none();
        }
        let mut subs = vec![
            iced::window::close_requests().map(Message::CloseRequested),
            Subscription::run(tray::actions).map(Message::Tray),
            iced::system::theme_changes().map(Message::Theme),
            iced::window::events().filter_map(|(_, e)| match e {
                iced::window::Event::Focused => Some(Message::Focus(true)),
                iced::window::Event::Unfocused => Some(Message::Focus(false)),
                _ => None,
            }),
        ];
        // Granting a permission in System Settings sends no event, so poll while waiting.
        if matches!(self.screen, Screen::Permissions { .. }) {
            subs.push(iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::PollPermissions));
        }
        if let Screen::Running { state, .. } = &self.screen {
            subs.push(Subscription::run(watch_state));
            // The pairing window lasts 120 s; renew it before it closes, while still wanted.
            if self.pairing_wanted() {
                subs.push(iced::time::every(std::time::Duration::from_secs(90)).map(|_| Message::KeepPairingOpen));
            }
            // Animate the ribbon only while the cursor is on another device.
            if matches!(
                state.phase,
                Phase::Connected {
                    mode: Mode::Driving(_) | Mode::Driven(_),
                    ..
                }
            ) {
                subs.push(iced::window::frames().map(|_| Message::Frame));
            }
        }
        Subscription::batch(subs)
    }

    /// The brand theme for the current light or dark mode.
    fn theme(&self) -> Option<Theme> {
        Some(style::theme(self.theme))
    }

    /// The backdrop, with the current screen on top: the logo as hero, or on the connected
    /// screen a compact header above the Desk map.
    fn view(&self) -> Element<'_, Message> {
        let body: Element<'_, Message> = match &self.screen {
            Screen::Permissions {
                input_monitoring,
                accessibility,
            } => permissions_view(*input_monitoring, *accessibility),
            Screen::Starting => text("Starting…").style(muted).into(),
            Screen::Failed(e) => column![
                title("Overdesk couldn't start".into()),
                text(e.as_str()).style(muted).align_x(Alignment::Center),
            ]
            .spacing(10)
            .align_x(Alignment::Center)
            .into(),
            Screen::Running { state, .. } => self.running_view(state),
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
        let here = device_word(state.os);
        match &state.phase {
            Phase::Idle => self.idle_view(state),
            Phase::Connecting { address } => column![
                title(format!("Connecting to {}…", address.ip())),
                subtitle("Waiting for the other computer to answer."),
                Space::new().height(18),
                button(text("Cancel"))
                    .padding([10, 22])
                    .style(style::secondary)
                    .on_press(Message::Send(Command::Disconnect)),
            ]
            .spacing(8)
            .align_x(Alignment::Center)
            .into(),
            Phase::Pairing { peer, code } => column![
                title("Check the code".into()),
                subtitle(format!("Make sure {peer} shows the same code before you continue.")),
                Space::new().height(18),
                text(format!("{:03} {:03}", code / 1000, code % 1000))
                    .size(56)
                    .font(bold()),
                Space::new().height(18),
                if self.answered {
                    Element::from(subtitle(format!("Waiting for {peer} to confirm…")))
                } else {
                    row![
                        button(text("Codes match"))
                            .padding([11, 26])
                            .style(style::primary)
                            .on_press(Message::Send(Command::AnswerCode(true))),
                        button(text("Cancel"))
                            .padding([11, 26])
                            .style(style::secondary)
                            .on_press(Message::Send(Command::AnswerCode(false))),
                    ]
                    .spacing(10)
                    .into()
                },
            ]
            .spacing(6)
            .align_x(Alignment::Center)
            .into(),
            Phase::Connected { peer, peer_os, mode } => {
                let status = match mode {
                    Mode::Local => format!("Cursor is on this {here}"),
                    Mode::Driving(_) => format!("Cursor is on {peer}"),
                    Mode::Driven(_) => format!("{peer} is using this {here}"),
                };
                let hint = match mode {
                    Mode::Driving(_) => format!("Press {} to take back control at any time.", escape_chord(state.os)),
                    Mode::Driven(_) => format!("{peer} is controlling this {here}."),
                    _ => format!("Move the cursor across the screen edge to use {peer}."),
                };
                column![
                    container(
                        canvas(desk::DeskMap {
                            screens: &state.desk,
                            mode: *mode,
                            phase: self.phase
                        })
                        .width(Fill)
                        .height(250)
                    )
                    .padding(6)
                    .style(style::panel),
                    Space::new().height(16),
                    text(status).size(17).font(bold()),
                    text(hint).style(muted),
                    Space::new().height(18),
                    row![
                        column![
                            text(peer.as_str()).font(bold()),
                            row![
                                text("●").size(11).color(style::TEAL),
                                text(format!("{}, connected", os_word(*peer_os))).style(muted)
                            ]
                            .spacing(6)
                            .align_y(Alignment::Center),
                        ]
                        .spacing(2),
                        Space::new().width(Fill),
                        button(text("Disconnect"))
                            .padding([10, 22])
                            .style(style::secondary)
                            .on_press(Message::Send(Command::Disconnect)),
                    ]
                    .align_y(Alignment::Center),
                ]
                .spacing(4)
                .into()
            }
            Phase::Lost { peer } => column![
                title(format!("Lost connection to {peer}")),
                subtitle(format!("Your keyboard and mouse are back on this {here}.")),
                Space::new().height(18),
                button(text("Done"))
                    .padding([10, 22])
                    .style(style::secondary)
                    .on_press(Message::Send(Command::Disconnect)),
            ]
            .spacing(8)
            .align_x(Alignment::Center)
            .into(),
            Phase::Error(message) => column![
                title("That didn't work".into()),
                subtitle(message.as_str()),
                Space::new().height(18),
                button(text("Try again"))
                    .padding([10, 22])
                    .style(style::primary)
                    .on_press(Message::Send(Command::Disconnect)),
            ]
            .spacing(8)
            .align_x(Alignment::Center)
            .into(),
        }
    }

    /// "Pair a device": which side the other computer is on, the computers found on the
    /// network, and a collapsed fallback for typing an address.
    fn idle_view<'a>(&'a self, state: &'a AppState) -> Element<'a, Message> {
        let here = device_word(state.os);
        let side = |s: Side, label: &'static str| {
            button(text(label))
                .padding([8, 20])
                .style(style::segment(self.side == s))
                .on_press(Message::Side(s))
        };
        let nearby: Element<'a, Message> = if state.nearby.is_empty() {
            column![
                text("Looking for computers on your network…").font(bold()),
                text("Open Overdesk on your other computer. It will appear here.").style(muted),
            ]
            .spacing(4)
            .width(Fill)
            .align_x(Alignment::Center)
            .into()
        } else {
            column(state.nearby.iter().map(|d| {
                let go = button(text(if d.paired { "Connect" } else { "Pair" }))
                    .padding([8, 20])
                    .style(style::primary)
                    .on_press(Message::Send(Command::Connect {
                        address: d.address,
                        side: self.side,
                        // Hold a paired device to its pinned identity; a first pairing has none.
                        expect: d.paired.then_some(d.id),
                    }));
                let mut info = row![text(os_word(d.os)).style(muted)].spacing(8);
                if d.paired {
                    info = info.push(text("Paired").color(style::TEAL));
                }
                let initial = d.name.chars().next().map_or(' ', |c| c.to_ascii_uppercase());
                let initial = if d.os == Os::Windows {
                    'W'
                } else if d.os == Os::MacOs {
                    'M'
                } else {
                    initial
                };
                container(
                    row![
                        container(text(initial.to_string()).size(15).font(bold()))
                            .center(38)
                            .style(style::glyph),
                        column![text(d.name.as_str()).font(bold()), info].spacing(2),
                        Space::new().width(Fill),
                        go,
                    ]
                    .spacing(14)
                    .align_y(Alignment::Center),
                )
                .padding([12, 16])
                .width(Fill)
                .style(style::panel)
                .into()
            }))
            .spacing(10)
            .into()
        };

        let valid = parse_address(&self.address).is_some();
        let connect = button(text("Connect")).padding([9, 22]).style(style::primary);
        let connect = if valid {
            connect.on_press(Message::Connect)
        } else {
            connect
        };
        let mut fallback = column![
            button(
                text(if self.show_address {
                    "Hide address"
                } else {
                    "Enter an address instead"
                })
                .size(13)
            )
            .padding([6, 12])
            .style(style::secondary)
            .on_press(Message::ToggleAddress)
        ]
        .spacing(10)
        .align_x(Alignment::Center);
        if self.show_address {
            fallback = fallback.push(
                row![
                    text_input("192.168.1.50", &self.address)
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

        column![
            title("Pair a device".into()),
            subtitle(format!("Share this {here}'s keyboard and mouse with another computer.")),
            Space::new().height(10),
            text(format!("Where is it, next to this {here}?")).size(13).style(muted),
            row![side(Side::Left, "On the left"), side(Side::Right, "On the right")].spacing(8),
            Space::new().height(6),
            // Two rows fit without scrolling; more scroll inside a fixed band so the
            // fallback button never falls off the bottom of the window.
            column![
                text("Nearby computers").font(bold()),
                container(scrollable(nearby).spacing(4)).max_height(NEARBY_MAX_HEIGHT)
            ]
            .spacing(8)
            .width(Fill),
            fallback,
        ]
        .spacing(6)
        .align_x(Alignment::Center)
        .into()
    }
}

/// A screen title: large and semibold.
fn title<'a>(s: String) -> iced::widget::Text<'a> {
    text(s).size(29).font(bold()).align_x(Alignment::Center)
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
        title("Overdesk needs two permissions".into()),
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
            button(text("Reopen Overdesk"))
                .padding([10, 22])
                .style(style::primary)
                .on_press(Message::Relaunch),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(10)
    .width(Length::Fill)
    .align_x(Alignment::Center)
    .into()
}

/// Streams each new controller state to the app as [`Message::State`].
fn watch_state() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(16, async |mut out: iced::futures::channel::mpsc::Sender<Message>| {
        let Some(rx) = STATE.get() else { return };
        let mut rx = rx.clone();
        // `changed` fails once the controller drops its sender; the stream then ends.
        while rx.changed().await.is_ok() {
            let now = rx.borrow_and_update().clone();
            if out.send(Message::State(now)).await.is_err() {
                return;
            }
        }
    })
}

/// Starts the native platform and the controller, and hands back its command sender
/// and first state.
async fn start() -> Result<(mpsc::UnboundedSender<Command>, AppState), String> {
    let platform = overdeskd::native::NativePlatform::start()?;
    let (width, height) = overdeskd::native::main_display_size();
    let config = controller::Config {
        store_dir: store_dir(),
        listen: SocketAddr::from(([0, 0, 0, 0], PORT)),
        discovery: true,
        me: Me {
            name: device_name(),
            os: this_os(),
            width,
            height,
        },
    };
    let (commands, state) = controller::spawn(config, platform).await.map_err(|e| e.to_string())?;
    let now = state.borrow().clone();
    // The subscription reads the state from here; it is set once per launch.
    let _ = STATE.set(state);
    Ok((commands, now))
}

/// Parses `ip:port`, or a bare IP on the default port.
fn parse_address(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    s.parse::<SocketAddr>()
        .ok()
        .or_else(|| s.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, PORT)))
}

/// The operating system this build runs on.
fn this_os() -> Os {
    if cfg!(windows) { Os::Windows } else { Os::MacOs }
}

/// What people call this device in sentences: "Mac" or "PC".
fn device_word(os: Os) -> &'static str {
    match os {
        Os::MacOs => "Mac",
        _ => "PC",
    }
}

/// A peer's operating system, named for people.
fn os_word(os: Os) -> &'static str {
    match os {
        Os::MacOs => "Mac",
        Os::Windows => "Windows PC",
        Os::Linux => "Linux PC",
        Os::Android => "Android TV",
    }
}

/// The key chord that always takes control back on the given operating system.
fn escape_chord(os: Os) -> &'static str {
    if os == Os::MacOs {
        "Ctrl+Option+Cmd+Esc"
    } else {
        "Ctrl+Alt+Win+Esc"
    }
}

/// Where the identity and pinned peers live: `OVERDESK_HOME`, or the per-OS default.
fn store_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("OVERDESK_HOME") {
        return dir.into();
    }
    let home = std::path::PathBuf::from(
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("APPDATA"))
            .unwrap_or_else(|| ".".into()),
    );
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Overdesk")
    } else if cfg!(windows) {
        home.join("Overdesk")
    } else {
        home.join(".overdesk")
    }
}

/// This device's name as people know it: the macOS computer name, or the Windows one.
fn device_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        && out.status.success()
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This computer".into())
}

/// Whether Input Monitoring and Accessibility are granted.
#[cfg(target_os = "macos")]
fn permissions() -> (bool, bool) {
    overdeskd::native::permissions()
}

/// Other operating systems need no permissions, so both count as granted.
#[cfg(not(target_os = "macos"))]
fn permissions() -> (bool, bool) {
    (true, true)
}

/// Opens System Settings at the pane that grants `which`, on macOS.
fn open_settings(which: Permission) {
    #[cfg(target_os = "macos")]
    {
        overdeskd::native::request_permissions();
        let pane = match which {
            Permission::InputMonitoring => "Privacy_ListenEvent",
            Permission::Accessibility => "Privacy_Accessibility",
        };
        let _ = std::process::Command::new("open")
            .arg(format!(
                "x-apple.systempreferences:com.apple.preference.security?{pane}"
            ))
            .spawn();
    }
    // Other platforms have no settings pane to open.
    let _ = which;
}

/// Starts a fresh process, since permissions take effect for a new process only. Inside a
/// macOS `.app` bundle it goes through `open -n`, so LaunchServices attributes the
/// permissions to the bundle rather than to the terminal or parent that started it.
fn relaunch() {
    if let Ok(exe) = std::env::current_exe() {
        let path = exe.to_string_lossy();
        let bundle = if cfg!(target_os = "macos") {
            path.split_once(".app/Contents/MacOS/")
                .map(|(prefix, _)| format!("{prefix}.app"))
        } else {
            None
        };
        let _ = match bundle {
            Some(bundle) => std::process::Command::new("open").arg("-n").arg(bundle).spawn(),
            None => std::process::Command::new(exe).spawn(),
        };
    }
    std::process::exit(0);
}
