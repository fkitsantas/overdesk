// SPDX-License-Identifier: AGPL-3.0-only
//! The Overdesk app: one quiet window around the controller. It renders the state the
//! controller publishes and sends it commands; capture, the engine and every failsafe
//! live in Rust below it, so the UI can never lock anyone out.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
mod backdrop;
mod desk;
mod diag;
mod ext;
mod form;
mod host;
mod icons;
mod login;
#[cfg(feature = "preview")]
mod preview;
mod style;
mod sys;
mod tray;
mod view;

use ext::Request;
use form::{Activation, AddressForm};
use sys::{open_settings, permissions, relaunch, start};
use view::PIN_DIGITS;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use iced::futures::SinkExt;
use iced::theme::Mode as ThemeMode;
use iced::{Font, Subscription, Task, Theme};
use overdesk_proto::PeerId;
use overdeskd::controller::{self, AppState, Command, PairedDevice, Phase, StartError};
use overdeskd::native::NativePlatform;
use overdeskd::{Mode, Side};
use tokio::sync::{mpsc, watch};

/// The port the controller listens on, and the one assumed when an address has none.
const PORT: u16 = 24801;
// Inter (SIL OFL 1.1, assets/fonts/Inter-LICENSE.txt): the grotesk of the wordmark,
// identical on macOS and Windows, with a real semibold.
/// The app's typeface, bundled so every platform renders the same.
const INTER: Font = Font::with_name("Inter");

/// How long the controller gets to say goodbye and release input before the process ends.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);
/// How often the ribbon animation redraws (about 30 frames a second).
const FRAME_EVERY: Duration = Duration::from_millis(33);
/// Full turns of the ribbon animation's phase per second.
const PHASE_PER_SEC: f64 = 2.4;

/// The controller's live state, read by the subscription (which takes no captures).
static STATE: OnceLock<watch::Receiver<AppState>> = OnceLock::new();

/// What starts the controller: the contract of [`controller::spawn`], with the future boxed so
/// that any async function (or a non-capturing closure around one) fits a plain `fn` pointer.
pub type Spawn = fn(controller::Config, NativePlatform) -> SpawnFuture;

/// The future a [`Spawn`] returns: the controller's command sender and live state.
pub type SpawnFuture = Pin<
    Box<dyn Future<Output = Result<(mpsc::UnboundedSender<Command>, watch::Receiver<AppState>), StartError>> + Send>,
>;

/// The controller starter chosen by the caller of [`run`], read when the app boots.
static SPAWN: OnceLock<Spawn> = OnceLock::new();

/// What the build knows about the user's licence. The open app has no licence UI; the
/// private build supplies the checking, so no key logic lives here.
#[derive(Debug, Clone)]
pub struct Edition {
    /// "Overdesk Pro" when a valid key is active, else `None` (Free): the licence email.
    pub licensed_to: Option<String>,
    /// Starts "Activate Pro": the page to open in the browser, the code it shows, and the wait
    /// for the licence (`Ok(email)` once it is stored, `Err` with a sentence for the user).
    pub activate: fn() -> ActivationStart,
    /// Describes the edition for diagnostics, given how many computers are linked now.
    pub report: fn(usize) -> EditionReport,
}

/// An "Activate Pro" that has started: the browser page where the user signs in and approves
/// this computer, the short code both screens show, and the wait for the licence to arrive.
pub struct ActivationStart {
    /// The overdesk.org page to open.
    pub url: String,
    /// The code the page also shows ("AB12-CD34"), so the user can tell it is this request.
    pub code: String,
    /// Resolves with the licensed email once the licence is stored, or why it was not.
    pub done: Pin<Box<dyn Future<Output = Result<String, String>> + Send>>,
}

impl std::fmt::Debug for ActivationStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivationStart")
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}

/// What a build says about itself in a bug report: asked when the report is made, so a licence that
/// changed since start-up is reported as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionReport {
    /// Whether Pro features are on right now.
    pub pro: bool,
    /// The version of the app the user runs (a build that wraps this one reports its own).
    pub version: &'static str,
    /// Extra lines for the diagnostics header, without the leading `# `. Never keys, tokens or emails.
    pub lines: Vec<String>,
}

/// The open app's [`Edition::report`]: Free, at this crate's version, with no extra lines.
pub fn free_report(_links: usize) -> EditionReport {
    EditionReport {
        pro: false,
        version: env!("CARGO_PKG_VERSION"),
        lines: Vec::new(),
    }
}

/// The edition chosen by the caller of [`run_with`]; absent for Free.
static EDITION: OnceLock<Edition> = OnceLock::new();

/// The extension chosen by the caller of [`run_with`]; absent in the open app.
static EXTENSION: OnceLock<Arc<dyn Extension>> = OnceLock::new();

/// What the extension asked for, drained by the app's subscription once.
static REQUESTS: Mutex<Option<mpsc::UnboundedReceiver<Request>>> = Mutex::new(None);

/// Opens the window as [`run`] does, with a licence row for `edition` and, when given, the
/// `extension`, if any. The controller is chosen at start-up, so a newly activated key
/// applies after a restart.
pub fn run_with(spawn: Spawn, edition: Edition, extension: Option<Arc<dyn Extension>>) -> iced::Result {
    let _ = EDITION.set(edition);
    if let Some(extension) = extension {
        let _ = EXTENSION.set(extension);
    }
    run(spawn)
}

/// Opens the window: Inter as the default font, the brand theme, and the controller's
/// state as a subscription. `spawn` starts the controller, so another build can bring its own.
pub fn run(spawn: Spawn) -> iced::Result {
    overdesk_proto::log::init(&store_dir());
    let report = EDITION.get().map_or_else(|| free_report(0), |e| (e.report)(0));
    overdesk_proto::log::info(format_args!(
        "Overdesk {} ({}) on {}, {}",
        report.version,
        if report.pro { "Pro" } else { "Free" },
        overdeskd::os_version(),
        std::env::consts::ARCH
    ));
    let _ = SPAWN.set(spawn);
    if let Some(extension) = EXTENSION.get() {
        let (host, requests) = Host::channel();
        *REQUESTS.lock().unwrap_or_else(PoisonError::into_inner) = Some(requests);
        extension.startup(host); // may end the process
    }
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
        // Opened at login (`--background`): stay in the tray until the user asks for the window.
        visible: !background(),
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

/// The argument the login item passes: start hidden in the tray instead of opening the window.
pub const BACKGROUND_ARG: &str = "--background";

/// Whether this run was started at login, to sit in the tray.
fn background() -> bool {
    std::env::args().skip(1).any(|a| a == BACKGROUND_ARG)
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
#[allow(clippy::large_enum_variant)] // one value for the app's whole life: boxing it would only add indirection
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
    /// The address fallback and the choice of side.
    form: AddressForm,
    /// The ribbon animation's position, 0.0..1.0, a function of time (see [`Message::Frame`]).
    phase: f32,
    /// When the app started: the animation's clock runs from here.
    epoch: Instant,
    /// Whether the window is on screen. A hidden window animates nothing.
    visible: bool,
    /// Whether the user has answered the pairing code and is waiting for the peer.
    /// Reset whenever the phase leaves [`Phase::Pairing`].
    answered: bool,
    /// The PIN being typed while a remote device shows one ([`Phase::EnterPin`]).
    pin: String,
    /// Whether the "Paired devices" section is expanded.
    show_paired: bool,
    /// The paired device whose Forget is waiting for the user's confirmation.
    confirm_forget: Option<PeerId>,
    /// Whether the window has focus. Pairing opens only while it does: pairing needs
    /// both people looking at both screens anyway, and an unattended machine must
    /// never let a LAN device pop a code prompt. Starts `false`, so it fails closed.
    focused: bool,
    /// The menu-bar / system-tray icon, once created (never in a preview).
    tray: Option<tray::Tray>,
    /// The build's extension, if any.
    ext: Option<Arc<dyn Extension>>,
    /// The status line the extension published.
    status: Option<StatusLine>,
    /// Whether Overdesk is set to open at login.
    login: bool,
    /// Why the last change to "Open at login" failed, until the next attempt.
    login_error: Option<String>,
    /// The licence key row.
    licence: Activation,
    /// A notice from the app itself (not the controller), shown until dismissed.
    notice: Option<String>,
    /// What each gesture a Mac reports does on this computer, and the Mac that made the choices appear.
    gestures: overdeskd::gestures::Settings,
}

/// Everything that can change the app: controller events, system events and user input.
#[derive(Debug, Clone)]
enum Message {
    /// The user chose what a gesture does.
    Gesture(overdeskd::gestures::Trigger, overdeskd::gestures::GestureAction),
    /// The controller started, or failed to.
    Started(Result<(mpsc::UnboundedSender<Command>, AppState), String>),
    /// The controller published a new state.
    State(AppState),
    /// The system switched between light and dark.
    Theme(ThemeMode),
    /// An animation tick, at the given time, while the cursor is on another device.
    Frame(Instant),
    /// Show or hide the address fallback.
    ToggleAddress,
    /// Show or hide the "Paired devices" section.
    TogglePaired,
    /// The user pressed Forget on a paired device: ask before doing it.
    AskForget(PeerId),
    /// The user backed out of forgetting.
    CancelForget,
    /// The user confirmed: forget the device being asked about.
    ConfirmForget,
    /// Re-open the pairing window while it is still wanted (see [`App::pairing_wanted`]).
    KeepPairingOpen,
    /// Create the tray icon. Queued from `boot` so it runs on the main thread inside the
    /// event loop, which macOS requires.
    CreateTray,
    /// The extension asked for something.
    Host(Request),
    /// The user pressed the button of the extension's status line.
    ExtensionButton,
    /// The user dismissed the app's own notice.
    DismissNotice,
    /// The user flipped "Open at login", in the window or the tray menu.
    ToggleLogin,
    /// The user picked an item from the tray menu.
    Tray(tray::Action),
    /// The user closed the window: hide it instead of quitting.
    CloseRequested(iced::window::Id),
    /// The window gained (`true`) or lost (`false`) focus.
    Focus(bool),
    /// The address field changed.
    Address(String),
    /// The PIN field changed.
    Pin(String),
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
    /// The user chose Activate Pro: open the browser and wait for the licence.
    Activate,
    /// The user stopped waiting for the browser.
    CancelActivate,
    /// The wait for the licence ended: the licensed email, or why not.
    Activated(Result<String, String>),
    /// Restart the app so newly granted permissions take effect.
    Relaunch,
    /// Try starting the controller again after it failed.
    Retry,
    /// Leave the app (from the failed screen, where there is no tray menu to use).
    Quit,
    /// A diagnostics file was saved (or could not be) for a bug report or on request.
    Diagnosed(Result<diag::Saved, String>),
    /// The preview screenshot is ready to save.
    #[cfg(feature = "preview")]
    Shot(iced::window::Screenshot),
}

impl App {
    /// An app showing `screen` in `theme`, with nothing typed, expanded or pending.
    fn new(screen: Screen, theme: ThemeMode) -> Self {
        Self {
            screen,
            theme,
            form: AddressForm::new(),
            phase: 0.0,
            epoch: Instant::now(),
            visible: !background(),
            answered: false,
            pin: String::new(),
            show_paired: false,
            confirm_forget: None,
            focused: false,
            tray: None,
            ext: EXTENSION.get().cloned(),
            status: None,
            login: false,
            login_error: None,
            licence: Activation::default(),
            notice: None,
            gestures: overdeskd::gestures::Settings::load(&store_dir()),
        }
    }

    /// Builds the first screen: the permissions check on macOS, otherwise starting the
    /// controller. With `OVERDESK_PREVIEW` set, renders a fixed preview state instead.
    fn boot() -> (Self, Task<Message>) {
        #[cfg(feature = "preview")]
        if let Ok(which) = std::env::var("OVERDESK_PREVIEW") {
            let (state, perms) = preview::state(&which);
            if let Some(edition) = preview::edition(&which) {
                let _ = EDITION.set(edition);
            }
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
                phase: 0.3,
                licence: Activation {
                    error: (which == "licence-error").then(|| "There is no active Pro licence on that account.".into()),
                    ..Activation::default()
                },
                ..Self::new(screen, preview::theme())
            };
            return (app, preview::shoot());
        }
        login::apply_default(&store_dir());
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
            login: login::is_enabled(),
            ..Self::new(screen, ThemeMode::Light)
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
                if let Some(ext) = &self.ext {
                    ext.on_state(&state);
                }
                self.screen = Screen::Running { commands, state };
            }
            Message::Started(Err(e)) => self.screen = Screen::Failed(e),
            Message::Retry => {
                self.screen = Screen::Starting;
                return Task::perform(start(), Message::Started);
            }
            Message::Quit => return iced::exit(),
            Message::State(new) => self.on_state(new),
            Message::PollPermissions => return self.poll_permissions(),
            Message::Theme(mode) => self.theme = mode,
            Message::Frame(at) => {
                // f64: an f32 clock loses the fraction of a turn after a few hours of uptime.
                self.phase = ((at.saturating_duration_since(self.epoch).as_secs_f64() * PHASE_PER_SEC) % 1.0) as f32;
            }
            Message::ToggleAddress => self.form.toggle(),
            Message::TogglePaired => self.show_paired = !self.show_paired,
            Message::AskForget(id) => self.confirm_forget = Some(id),
            Message::CancelForget => self.confirm_forget = None,
            Message::ConfirmForget => {
                if let Some(id) = self.confirm_forget.take() {
                    self.send(Command::Forget(id));
                }
            }
            Message::Focus(f) => {
                self.focused = f;
                // A focused window is on screen, whatever hid it (the tray cannot always say).
                self.visible |= f;
            }
            Message::DismissNotice => self.notice = None,
            Message::Gesture(trigger, action) => {
                self.gestures.map.set(trigger, action);
                self.save_gestures();
            }
            // The subscription only exists while wanted, but a queued tick may outlive it.
            Message::KeepPairingOpen => {
                if self.pairing_wanted() {
                    self.send(Command::AllowPairing(true));
                }
            }
            Message::Address(a) => self.form.address = a,
            Message::Side(s) => self.form.side = s,
            Message::Connect => {
                if let Some(address) = self.form.target() {
                    self.send(Command::Connect {
                        address,
                        side: self.form.side,
                        expect: None, // a typed address has no earlier identity to hold it to
                    });
                }
            }
            // Digits only, as many as a PIN has.
            Message::Pin(p) => self.pin = p.chars().filter(char::is_ascii_digit).take(PIN_DIGITS).collect(),
            Message::Send(c) => {
                self.answered |= matches!(c, Command::AnswerCode(_) | Command::Answer { .. });
                self.send(c);
            }
            Message::Activate => {
                if let Some(edition) = EDITION.get() {
                    return self.licence.start(edition);
                }
            }
            Message::CancelActivate => self.licence.cancel(),
            Message::Activated(result) => self.licence.finish(result),
            Message::OpenSettings(p) => open_settings(p),
            Message::Relaunch => relaunch(),
            #[cfg(feature = "preview")]
            Message::Shot(shot) => preview::save(shot),
            // The tray, the login item, closing and the extension (host.rs).
            m @ (Message::CreateTray
            | Message::CloseRequested(_)
            | Message::Tray(_)
            | Message::Host(_)
            | Message::ExtensionButton
            | Message::Diagnosed(_)
            | Message::ToggleLogin) => return self.apply_host(m),
        }
        Task::none()
    }

    /// Takes a state the controller published.
    fn on_state(&mut self, new: AppState) {
        self.drop_stale(&new.phase, &new.paired);
        if let Some(ext) = &self.ext {
            ext.on_state(&new);
        }
        self.remember_gesture_mac(&new.phase);
        if let Screen::Running { state, .. } = &mut self.screen {
            *state = new;
            if let Some(tray) = &self.tray {
                tray.sync(state);
            }
        }
    }

    /// Remembers a connected Mac that reports a pointing device, so its gesture choices stay
    /// listed after it leaves.
    fn remember_gesture_mac(&mut self, phase: &Phase) {
        let Phase::Connected { peers, .. } = phase else { return };
        let mac = peers
            .iter()
            .find(|p| overdeskd::gestures::reports_pointing(p.os, p.features));
        if let Some(mac) = mac.filter(|m| self.gestures.seen.as_deref() != Some(m.name.as_str())) {
            self.gestures.seen = Some(mac.name.clone());
            self.save_gestures();
        }
    }

    /// Saves the gesture choices (they already apply); a failure only costs them at the next start.
    fn save_gestures(&self) {
        if let Err(e) = self.gestures.save(&store_dir()) {
            overdesk_proto::log::warn(format_args!("could not save the gesture choices: {e}"));
        }
    }

    /// Drops what a new `phase` and list of `paired` devices made stale.
    fn drop_stale(&mut self, phase: &Phase, paired: &[PairedDevice]) {
        if !matches!(phase, Phase::Pairing { .. }) {
            self.answered = false;
        }
        if !matches!(phase, Phase::EnterPin { .. }) {
            self.pin.clear();
        }
        // A device that is gone (forgotten here or by the other side) has nothing left to confirm.
        if self.confirm_forget.is_some_and(|id| !paired.iter().any(|d| d.id == id)) {
            self.confirm_forget = None;
        }
    }

    /// Re-reads the macOS permissions and starts the controller once both are granted.
    fn poll_permissions(&mut self) -> Task<Message> {
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
        // A preview renders one frozen state; a live state would overwrite it.
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
        if self.ext.is_some() {
            subs.push(Subscription::run(host_requests).map(Message::Host));
        }
        // Granting a permission in System Settings sends no event, so poll while waiting.
        if matches!(self.screen, Screen::Permissions { .. }) {
            subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::PollPermissions));
        }
        if let Screen::Running { state, .. } = &self.screen {
            subs.push(Subscription::run(watch_state));
            // The pairing window lasts 120 s; renew it before it closes, while still wanted.
            if self.pairing_wanted() {
                subs.push(iced::time::every(Duration::from_secs(90)).map(|_| Message::KeepPairingOpen));
            }
            // Animate the ribbon only while the cursor is on another device and the window shows.
            let away = matches!(
                state.phase,
                Phase::Connected {
                    mode: Mode::Driving(_) | Mode::Driven(_),
                    ..
                }
            );
            if away && self.visible {
                subs.push(iced::time::every(FRAME_EVERY).map(Message::Frame));
            }
        }
        Subscription::batch(subs)
    }

    /// The brand theme for the current light or dark mode.
    fn theme(&self) -> Option<Theme> {
        Some(style::theme(self.theme))
    }
}

/// Streams what the extension asks for as [`Message::Host`].
fn host_requests() -> impl iced::futures::Stream<Item = Request> {
    iced::stream::channel(16, async |mut out: iced::futures::channel::mpsc::Sender<Request>| {
        let taken = REQUESTS.lock().unwrap_or_else(PoisonError::into_inner).take();
        let Some(mut rx) = taken else { return };
        while let Some(request) = rx.recv().await {
            if out.send(request).await.is_err() {
                return;
            }
        }
    })
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

// Re-exported for the official build, which keeps its licence in the same folder as the identity.
pub use ext::{Extension, Host, MenuItemSpec, StatusLine};
pub use overdeskd::store_dir;

#[cfg(test)]
mod tests {
    use super::*;
    use overdesk_proto::Os;
    use overdeskd::controller::DeviceKind;

    /// A peer id made of one repeated byte.
    fn id_of(n: u8) -> PeerId {
        PeerId([n; 16])
    }

    #[test]
    fn forgetting_a_paired_device_asks_first_and_can_be_cancelled() {
        let id = id_of(5);
        let mut app = App::boot().0;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let state = AppState::new("Me".into(), Os::MacOs);
        app.screen = Screen::Running { commands: tx, state };
        let _ = app.apply(Message::AskForget(id));
        assert_eq!(app.confirm_forget, Some(id));
        assert!(rx.try_recv().is_err(), "asking forgets nothing");
        let _ = app.apply(Message::CancelForget);
        let _ = app.apply(Message::ConfirmForget);
        assert!(rx.try_recv().is_err(), "a cancelled question forgets nothing");
        let _ = app.apply(Message::AskForget(id));
        let _ = app.apply(Message::ConfirmForget);
        assert_eq!(rx.try_recv().unwrap(), Command::Forget(id));
        assert_eq!(app.confirm_forget, None);
    }

    #[test]
    fn a_pending_forget_is_dropped_when_the_device_leaves_the_paired_list() {
        let id = id_of(5);
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::boot().0;
        let mut state = AppState {
            paired: vec![overdeskd::controller::PairedDevice {
                id,
                name: "PC".into(),
                kind: DeviceKind::Computer,
                online: true,
                os: None,
            }],
            ..AppState::new("Mac".into(), Os::MacOs)
        };
        app.screen = Screen::Running {
            commands: tx,
            state: state.clone(),
        };
        let _ = app.apply(Message::AskForget(id));
        let _ = app.apply(Message::State(state.clone()));
        assert_eq!(app.confirm_forget, Some(id), "still paired: the question stays");
        state.paired.clear();
        let _ = app.apply(Message::State(state));
        assert_eq!(app.confirm_forget, None, "the other side forgot us meanwhile");
    }
    #[test]
    fn the_ribbon_phase_follows_the_clock_not_the_frame_count() {
        let mut app = App::boot().0;
        let t = app.epoch;
        let _ = app.apply(Message::Frame(t + Duration::from_millis(100)));
        assert!((app.phase - 0.24).abs() < 1e-3, "{}", app.phase);
        // 500 ms is 1.2 turns: the phase wraps.
        let _ = app.apply(Message::Frame(t + Duration::from_millis(500)));
        assert!((app.phase - 0.2).abs() < 1e-3, "{}", app.phase);
        // Days of uptime: 2.4 turns a second is a whole number of turns every 5 s.
        let _ = app.apply(Message::Frame(t + Duration::from_millis(1_000_000_100)));
        assert!((app.phase - 0.24).abs() < 1e-3, "{}", app.phase);
    }

    #[test]
    fn focusing_the_window_marks_it_visible() {
        let mut app = App::boot().0;
        app.visible = false;
        let _ = app.apply(Message::Focus(false));
        assert!(!app.visible);
        let _ = app.apply(Message::Focus(true));
        assert!(app.visible);
    }

    #[test]
    fn the_pin_field_takes_only_digits_and_no_more_than_a_pin_has() {
        let mut app = App::boot().0;
        let _ = app.apply(Message::Pin("12a3-45 6789".into()));
        assert_eq!(app.pin, "1234");
        let _ = app.apply(Message::Pin(String::new()));
        assert_eq!(app.pin, "");
    }
}
