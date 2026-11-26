// SPDX-License-Identifier: AGPL-3.0-only
//! The Overdesk app: one quiet window around the controller. It renders the state the
//! controller publishes and sends it commands; capture, the engine and every failsafe
//! live in Rust below it, so the UI can never lock anyone out.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
mod desk;
#[cfg(feature = "preview")]
mod preview;
mod style;

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::OnceLock;

use iced::futures::SinkExt;
use iced::theme::Mode as ThemeMode;
use iced::widget::{Space, button, canvas, column, container, row, svg, text, text_input};
use iced::{Alignment, Element, Fill, Font, Length, Subscription, Task, Theme, font};
use overdesk_core::engine::Mode;
use overdesk_net::session::Me;
use overdesk_proto::Os;
use overdeskd::Side;
use overdeskd::controller::{self, AppState, Command, Phase};
use tokio::sync::{mpsc, watch};

/// The port the controller listens on, and the one assumed when an address has none.
const PORT: u16 = 24801;
/// The Overdesk symbol, drawn beside the wordmark in the header.
const SYMBOL: &[u8] = include_bytes!("../../../assets/brand/symbol.svg");
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
        .theme(App::theme)
        .window_size((620.0, 500.0))
        .font(include_bytes!("../../../assets/fonts/Inter-Regular.ttf").as_slice())
        .font(include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf").as_slice())
        .default_font(INTER)
        .run()
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
        };
        let theme = iced::system::theme().map(Message::Theme);
        (
            app,
            if starting {
                Task::batch([theme, Task::perform(start(), Message::Started)])
            } else {
                theme
            },
        )
    }

    /// Applies a message. Connection changes happen only by sending controller commands.
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Started(Ok((commands, state))) => self.screen = Screen::Running { commands, state },
            Message::Started(Err(e)) => self.screen = Screen::Failed(e),
            Message::State(new) => {
                if let Screen::Running { state, .. } = &mut self.screen {
                    *state = new;
                }
            }
            Message::Theme(mode) => self.theme = mode,
            Message::Frame => self.phase = (self.phase + 0.04) % 1.0,
            Message::Address(a) => self.address = a,
            Message::Side(s) => self.side = s,
            Message::Connect => {
                if let Some(address) = parse_address(&self.address) {
                    self.send(Command::Connect {
                        address,
                        side: self.side,
                    });
                }
            }
            Message::Send(c) => self.send(c),
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
        let mut subs = vec![iced::system::theme_changes().map(Message::Theme)];
        if let Screen::Running { state, .. } = &self.screen {
            subs.push(Subscription::run(watch_state));
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

    /// The header above whichever screen is current.
    fn view(&self) -> Element<'_, Message> {
        let header = row![
            svg(svg::Handle::from_memory(SYMBOL)).width(34).height(16),
            text("Overdesk").size(17).font(bold()),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let body: Element<'_, Message> = match &self.screen {
            Screen::Permissions {
                input_monitoring,
                accessibility,
            } => permissions_view(*input_monitoring, *accessibility),
            Screen::Starting => text("Starting…").style(muted).into(),
            Screen::Failed(e) => column![
                text("Overdesk couldn't start").size(22).font(bold()),
                text(e.as_str()).style(muted),
            ]
            .spacing(8)
            .into(),
            Screen::Running { state, .. } => self.running_view(state),
        };

        container(column![header, Space::new().height(22), body].max_width(560))
            .padding(28)
            .center_x(Fill)
            .into()
    }

    /// The screen for each controller phase.
    fn running_view<'a>(&'a self, state: &'a AppState) -> Element<'a, Message> {
        let here = device_word(state.os);
        match &state.phase {
            Phase::Idle => self.idle_view(state),
            Phase::Connecting { address } => column![
                title(format!("Connecting to {}…", address.ip())),
                text("Waiting for the other computer to answer.").style(muted),
                Space::new().height(18),
                button(text("Cancel"))
                    .padding([9, 16])
                    .style(style::secondary)
                    .on_press(Message::Send(Command::Disconnect)),
            ]
            .spacing(8)
            .into(),
            Phase::Pairing { peer, code } => column![
                title("Check the code".into()),
                text(format!("Make sure {peer} shows the same code before you continue.")).style(muted),
                Space::new().height(26),
                text(format!("{:03} {:03}", code / 1000, code % 1000))
                    .size(64)
                    .font(bold()),
                Space::new().height(26),
                row![
                    button(text("Codes match"))
                        .padding([10, 18])
                        .style(style::primary)
                        .on_press(Message::Send(Command::AnswerCode(true))),
                    button(text("Cancel"))
                        .padding([10, 18])
                        .style(style::secondary)
                        .on_press(Message::Send(Command::AnswerCode(false))),
                ]
                .spacing(10),
            ]
            .spacing(6)
            .into(),
            Phase::Connected { peer, peer_os, mode } => {
                let status = match mode {
                    Mode::Local => format!("Cursor is on this {here}"),
                    Mode::Driving(_) => format!("Cursor is on {peer}"),
                    Mode::Driven(_) => format!("{peer} is using this {here}"),
                };
                let hint = match mode {
                    Mode::Driving(_) => format!("Press {} to take back control at any time.", escape_chord(state.os)),
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
                        .height(210)
                    )
                    .padding(6)
                    .style(style::panel),
                    Space::new().height(14),
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
                            .padding([9, 16])
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
                text(format!("Your keyboard and mouse are back on this {here}.")).style(muted),
                Space::new().height(18),
                button(text("Done"))
                    .padding([9, 16])
                    .style(style::secondary)
                    .on_press(Message::Send(Command::Disconnect)),
            ]
            .spacing(8)
            .into(),
            Phase::Error(message) => column![
                title("That didn't work".into()),
                text(message.as_str()).style(muted),
                Space::new().height(18),
                button(text("Try again"))
                    .padding([9, 16])
                    .style(style::secondary)
                    .on_press(Message::Send(Command::Disconnect)),
            ]
            .spacing(8)
            .into(),
        }
    }

    /// The pairing form: the other device's address, which side it is on, and this
    /// device's own address and pairing control.
    fn idle_view<'a>(&'a self, state: &'a AppState) -> Element<'a, Message> {
        let here = device_word(state.os);
        let side = |s: Side, label: &'static str| {
            button(text(label))
                .padding([9, 14])
                .style(style::segment(self.side == s))
                .on_press(Message::Side(s))
        };
        let valid = parse_address(&self.address).is_some();
        let connect = button(text("Connect")).padding([9, 18]).style(style::primary);
        let connect = if valid {
            connect.on_press(Message::Connect)
        } else {
            connect
        };

        // The controller binds 0.0.0.0; show the LAN address another device can actually dial.
        let my_address = state
            .address
            .map(|a| SocketAddr::new(lan_ip().unwrap_or(a.ip()), a.port()).to_string())
            .unwrap_or_else(|| "not listening".into());
        let pairing = if state.pairing_open {
            row![
                text("Pairing is open for 2 minutes.").style(muted),
                Space::new().width(Fill),
                button(text("Close"))
                    .padding([8, 14])
                    .style(style::secondary)
                    .on_press(Message::Send(Command::AllowPairing(false))),
            ]
        } else {
            row![
                text(format!("This {here}: {my_address}")).style(muted),
                Space::new().width(Fill),
                button(text("Allow pairing"))
                    .padding([8, 14])
                    .style(style::secondary)
                    .on_press(Message::Send(Command::AllowPairing(true))),
            ]
        };

        column![
            title("Pair a device".into()),
            text(format!("Open Overdesk on your other computer, then enter its address to share this {here}'s keyboard and mouse with it.")).style(muted),
            Space::new().height(18),
            container(
                column![
                    text("Address of the other computer").size(13).style(muted),
                    text_input("192.168.1.50", &self.address).on_input(Message::Address).on_submit(Message::Connect).padding(10).style(style::field),
                    Space::new().height(6),
                    text(format!("Where is it, next to this {here}?")).size(13).style(muted),
                    row![side(Side::Left, "On the left"), side(Side::Right, "On the right"), Space::new().width(Fill), connect]
                        .spacing(8)
                        .align_y(Alignment::Center),
                ]
                .spacing(8),
            )
            .padding(18)
            .style(style::panel),
            Space::new().height(18),
            pairing.align_y(Alignment::Center),
        ]
        .spacing(6)
        .into()
    }
}

/// A screen title: large and semibold.
fn title<'a>(s: String) -> iced::widget::Text<'a> {
    text(s).size(22).font(bold())
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
        .padding(16)
        .style(style::panel)
    };
    column![
        title("Overdesk needs two permissions".into()),
        text("macOS asks you to approve these once. Overdesk never records or stores what you type.").style(muted),
        Space::new().height(18),
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
        Space::new().height(18),
        row![
            text("After granting both, reopen Overdesk.").style(muted),
            Space::new().width(Fill),
            button(text("Reopen Overdesk"))
                .padding([9, 16])
                .style(style::primary)
                .on_press(Message::Relaunch),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(10)
    .width(Length::Fill)
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

/// This device's LAN address. Connecting a UDP socket only picks a route; it sends nothing.
fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?; // TEST-NET-1: documentation range, never routed
    socket.local_addr().ok().map(|a| a.ip())
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

/// Permissions take effect for a fresh process only.
fn relaunch() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}
