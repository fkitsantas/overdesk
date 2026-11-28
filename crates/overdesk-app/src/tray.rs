// SPDX-License-Identifier: AGPL-3.0-only
//! The menu-bar (macOS), system-tray (Windows) and notification-area (Linux) icon: a status line, a way back to the
//! window, Disconnect and Quit. Closing the window only hides it, so this icon is how
//! the app is reopened and ended.
//!
//! On Linux the icon is a StatusNotifierItem served over D-Bus from tray-icon's own worker
//! thread (its `ksni` backend), so no GTK main loop is needed next to iced. A desktop with
//! no StatusNotifier host (stock GNOME without the AppIndicator extension) shows no icon;
//! the window then still works, but closing it hides it with no way back, so see
//! `Message::CloseRequested`, which quits when creating the tray failed.

use std::cell::RefCell;

use crate::ext::MenuItemSpec;
use iced::futures::SinkExt;
use overdeskd::Mode;
use overdeskd::controller::{AppState, Phase, ends_attempt};
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// The monochrome template glyph, 36x36 for Retina menu bars, rendered from
/// `assets/brand/tray-template.svg`. macOS tints it for light and dark menu bars.
#[cfg(target_os = "macos")]
const ICON: &[u8] = include_bytes!("../../../assets/brand/tray-template-36.png");
/// The full-colour app icon at 32x32, rendered from `assets/brand/app-icon-square-1024.png`.
#[cfg(not(target_os = "macos"))]
const ICON: &[u8] = include_bytes!("../../../assets/brand/tray-win-32.png");

/// Menu item id of "Show Overdesk".
const SHOW: &str = "show";
/// Menu item id of "Disconnect".
const DISCONNECT: &str = "disconnect";
/// Menu item id of "Open at login".
const LOGIN: &str = "login";
/// Menu item id of "Quit Overdesk".
const QUIT: &str = "quit";

/// What the user picked from the tray menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Reopen or focus the window.
    Show,
    /// End the current session.
    Disconnect,
    /// An item the build's extension added, by its id.
    Menu(String),
    /// Flip "Open at login".
    ToggleLogin,
    /// Disconnect if needed and exit.
    Quit,
}

/// The live tray icon and the menu items that change with the controller's state.
///
/// Not `Send` on macOS: create and use it on the main thread only.
pub struct Tray {
    /// The icon itself; dropping it removes it from the menu bar.
    icon: TrayIcon,
    /// The disabled first line describing the connection.
    status: MenuItem,
    /// "Disconnect", enabled while connected or an attempt is live.
    disconnect: MenuItem,
    /// The checkable "Open at login".
    login: CheckMenuItem,
    /// The status line and Disconnect state last shown, so an unchanged state touches nothing.
    shown: RefCell<Option<(String, bool)>>,
}

impl Tray {
    /// Creates the icon and its menu, showing `state`. Must run on the main thread once
    /// the event loop is up (macOS requirement).
    pub fn new(state: Option<&AppState>, login: bool, extra: &[MenuItemSpec]) -> Result<Self, String> {
        #[cfg(free_unix)]
        if !host_present() {
            return Err("no StatusNotifier host on the session bus".into());
        }
        let status = MenuItem::new("Not connected", false, None);
        let disconnect = MenuItem::with_id(DISCONNECT, "Disconnect", false, None);
        let login = CheckMenuItem::with_id(LOGIN, "Open at login", true, login, None);
        let extra: Vec<MenuItem> = extra
            .iter()
            .map(|i| MenuItem::with_id(&i.id, &i.label, true, None))
            .collect();
        let (show, quit) = (
            MenuItem::with_id(SHOW, "Show Overdesk", true, None),
            MenuItem::with_id(QUIT, "Quit Overdesk", true, None),
        );
        let separator = PredefinedMenuItem::separator();
        let mut items: Vec<&dyn IsMenuItem> = vec![&status, &separator, &show, &disconnect, &separator];
        items.extend(extra.iter().map(|i| i as &dyn IsMenuItem));
        items.extend([&login as &dyn IsMenuItem, &quit]);
        let menu = Menu::with_items(&items).map_err(|e| e.to_string())?;
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Overdesk");
        let icon = decode(ICON)?;
        // The template flag lets macOS tint the glyph for light and dark menu bars.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(icon);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(icon);
        let icon = builder.build().map_err(|e| e.to_string())?;
        let tray = Self {
            icon,
            status,
            disconnect,
            login,
            shown: RefCell::new(None),
        };
        if let Some(state) = state {
            tray.sync(state);
        }
        Ok(tray)
    }

    /// Ticks or clears "Open at login" to match the real setting.
    pub fn set_login(&self, on: bool) {
        self.login.set_checked(on);
    }

    /// Reflects the controller's state in the status line, tooltip and Disconnect item.
    pub fn sync(&self, state: &AppState) {
        // Also while an attempt is live, so a stuck Connecting or an open PIN prompt can be ended.
        let now = (status_text(state), ends_attempt(&state.phase));
        let mut shown = self.shown.borrow_mut();
        if shown.as_ref() == Some(&now) {
            return; // every menu call is an OS call: only make them when something changed
        }
        self.status.set_text(&now.0);
        self.disconnect.set_enabled(now.1);
        let _ = self.icon.set_tooltip(Some(format!("Overdesk: {}", now.0)));
        *shown = Some(now);
    }
}

/// Whether a StatusNotifier host (the desktop's tray) is running. tray-icon's Linux backend
/// assumes one and would otherwise draw nothing while the window hid itself on close.
/// No session bus counts as no host. zbus 6 is async-only, so a throwaway runtime on its own
/// thread (never nested in one the caller may be inside) answers this one query.
#[cfg(free_unix)]
fn host_present() -> bool {
    async fn owned() -> zbus::Result<bool> {
        let bus = zbus::Connection::session().await?;
        let name = zbus::names::BusName::try_from("org.kde.StatusNotifierWatcher")?;
        zbus::fdo::DBusProxy::new(&bus)
            .await?
            .name_has_owner(name)
            .await
            .map_err(Into::into)
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .is_ok_and(|rt| rt.block_on(owned()).unwrap_or(false)),
        );
    });
    // A stuck session bus must not hold up the window: no answer soon counts as no host.
    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(false)
}

/// The status line for a state: "Not connected", "Connecting…", "Pairing…", "Waiting for the PIN
/// from {name}", "Couldn't connect" after an error, "Lost connection
/// to {peer}", "Connected to {peer}", "Using {peer}" while driving, or "{peer} is using
/// {this computer}" while driven.
fn status_text(state: &AppState) -> String {
    let Phase::Connected { peers, mode } = &state.phase else {
        return match &state.phase {
            Phase::Connecting { .. } => "Connecting…".into(),
            Phase::Pairing { .. } => "Pairing…".into(),
            Phase::EnterPin { name } => format!("Waiting for the PIN from {name}"),
            Phase::Error(_) => "Couldn't connect".into(),
            Phase::Lost { peer } => format!("Lost connection to {peer}"),
            _ => "Not connected".into(),
        };
    };
    let peer = peers.first().map_or("", |p| p.name.as_str());
    match mode {
        Mode::Local => format!("Connected to {peer}"),
        Mode::Driving(_) => format!("Using {peer}"),
        Mode::Driven(_) => {
            format!("{peer} is using {}", state.name)
        }
    }
}

/// Decodes an embedded 8-bit RGBA PNG into a tray icon.
fn decode(bytes: &[u8]) -> Result<Icon, String> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .map_err(|e| e.to_string())?;
    let mut rgba = vec![0; reader.output_buffer_size().ok_or("tray icon too large")?];
    let frame = reader.next_frame(&mut rgba).map_err(|e| e.to_string())?;
    rgba.truncate(frame.buffer_size());
    Icon::from_rgba(rgba, frame.width, frame.height).map_err(|e| e.to_string())
}

/// Streams the user's tray menu picks.
///
/// `MenuEvent::receiver()` is a blocking crossbeam channel. A dedicated blocking task
/// waits on it, so a pick reaches the UI the moment it happens, with no idle polling
/// and no timer waking the app several times a second.
pub fn actions() -> impl iced::futures::Stream<Item = Action> {
    iced::stream::channel(8, async |mut out: iced::futures::channel::mpsc::Sender<Action>| {
        loop {
            let Ok(Ok(event)) = tokio::task::spawn_blocking(|| MenuEvent::receiver().recv()).await else {
                return;
            };
            let action = match event.id.as_ref() {
                SHOW => Action::Show,
                DISCONNECT => Action::Disconnect,
                LOGIN => Action::ToggleLogin,
                QUIT => Action::Quit,
                other => Action::Menu(other.to_owned()),
            };
            if out.send(action).await.is_err() {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(phase: Phase) -> AppState {
        AppState {
            phase,
            ..AppState::new("Mac".into(), overdesk_proto::Os::MacOs)
        }
    }

    #[test]
    fn the_status_line_tells_each_phase_apart() {
        let addr = std::net::SocketAddr::from(([10, 0, 0, 1], 24801));
        let text = |p| status_text(&state(p));
        assert_eq!(text(Phase::Idle), "Not connected");
        assert_eq!(text(Phase::Error("x".into())), "Couldn't connect");
        assert_eq!(
            text(Phase::EnterPin { name: "TV".into() }),
            "Waiting for the PIN from TV"
        );
        assert_eq!(text(Phase::Connecting { address: addr }), "Connecting…");
        assert_eq!(
            text(Phase::Pairing {
                peer: "p".into(),
                code: 1
            }),
            "Pairing…"
        );
        assert_eq!(text(Phase::Lost { peer: "PC".into() }), "Lost connection to PC");
    }
}
