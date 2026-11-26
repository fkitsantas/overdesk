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

const PORT: u16 = 24801;
const SYMBOL: &[u8] = include_bytes!("../../../assets/brand/symbol.svg");
// Inter (SIL OFL 1.1, assets/fonts/Inter-LICENSE.txt): the grotesk of the wordmark,
// identical on macOS and Windows, with a real semibold.
const INTER: Font = Font::with_name("Inter");

/// The controller's live state, read by the subscription (which takes no captures).
static STATE: OnceLock<watch::Receiver<AppState>> = OnceLock::new();

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Permission {
    InputMonitoring,
    Accessibility,
}

enum Screen {
    /// macOS needs Input Monitoring and Accessibility before anything else.
    Permissions {
        input_monitoring: bool,
        accessibility: bool,
    },
    Starting,
    Failed(String),
    Running {
        commands: mpsc::UnboundedSender<Command>,
        state: AppState,
    },
}

struct App {
    screen: Screen,
    theme: ThemeMode,
    address: String,
    side: Side,
    phase: f32,
}

#[derive(Debug, Clone)]
enum Message {
    Started(Result<(mpsc::UnboundedSender<Command>, AppState), String>),
    State(AppState),
    Theme(ThemeMode),
    Frame,
    Address(String),
    Side(Side),
    Connect,
    Send(Command),
    OpenSettings(Permission),
    Relaunch,
    #[cfg(feature = "preview")]
    Shot(iced::window::Screenshot),
}

impl App {
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

    fn send(&self, c: Command) {
        if let Screen::Running { commands, .. } = &self.screen {
            let _ = commands.send(c);
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        if cfg!(feature = "preview") && std::env::var_os("OVERDESK_PREVIEW").is_some() {
            return Subscription::none();
        }
        let mut subs = vec![iced::system::theme_changes().map(Message::Theme)];
        if let Screen::Running { state, .. } = &self.screen {
            subs.push(Subscription::run(watch_state));
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

    fn theme(&self) -> Option<Theme> {
        Some(style::theme(self.theme))
    }

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

fn title<'a>(s: String) -> iced::widget::Text<'a> {
    text(s).size(22).font(bold())
}

fn bold() -> Font {
    Font {
        weight: font::Weight::Semibold,
        ..INTER
    }
}

fn muted(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(style::muted(theme)),
    }
}

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

fn watch_state() -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(16, async |mut out: iced::futures::channel::mpsc::Sender<Message>| {
        let Some(rx) = STATE.get() else { return };
        let mut rx = rx.clone();
        while rx.changed().await.is_ok() {
            let now = rx.borrow_and_update().clone();
            if out.send(Message::State(now)).await.is_err() {
                return;
            }
        }
    })
}

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
    let _ = STATE.set(state);
    Ok((commands, now))
}

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

fn this_os() -> Os {
    if cfg!(windows) { Os::Windows } else { Os::MacOs }
}

fn device_word(os: Os) -> &'static str {
    match os {
        Os::MacOs => "Mac",
        _ => "PC",
    }
}

fn os_word(os: Os) -> &'static str {
    match os {
        Os::MacOs => "Mac",
        Os::Windows => "Windows PC",
        Os::Linux => "Linux PC",
        Os::Android => "Android TV",
    }
}

fn escape_chord(os: Os) -> &'static str {
    if os == Os::MacOs {
        "Ctrl+Option+Cmd+Esc"
    } else {
        "Ctrl+Alt+Win+Esc"
    }
}

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

#[cfg(target_os = "macos")]
fn permissions() -> (bool, bool) {
    overdeskd::native::permissions()
}

#[cfg(not(target_os = "macos"))]
fn permissions() -> (bool, bool) {
    (true, true)
}

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
    let _ = which;
}

/// Permissions take effect for a fresh process only.
fn relaunch() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::process::exit(0);
}
