// SPDX-License-Identifier: AGPL-3.0-only
//! The menu-bar (macOS) and system-tray (Windows) icon: a status line, a way back to the
//! window, Disconnect and Quit. Closing the window only hides it, so this icon is how
//! the app is reopened and ended.

use iced::futures::SinkExt;
use overdesk_core::engine::Mode;
use overdesk_proto::Os;
use overdeskd::controller::{AppState, Phase};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
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
/// Menu item id of "Check for updates".
const UPDATES: &str = "updates";
/// Menu item id of "Open at login".
const LOGIN: &str = "login";
/// Menu item id of "Quit Overdesk".
const QUIT: &str = "quit";

/// What the user picked from the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Reopen or focus the window.
    Show,
    /// End the current session.
    Disconnect,
    /// Look for an update now.
    CheckUpdates,
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
    /// "Disconnect", enabled only while connected.
    disconnect: MenuItem,
    /// The checkable "Open at login".
    login: CheckMenuItem,
}

impl Tray {
    /// Creates the icon and its menu, showing `state`. Must run on the main thread once
    /// the event loop is up (macOS requirement).
    pub fn new(state: Option<&AppState>, login: bool) -> Result<Self, String> {
        let status = MenuItem::new("Not connected", false, None);
        let disconnect = MenuItem::with_id(DISCONNECT, "Disconnect", false, None);
        let login = CheckMenuItem::with_id(LOGIN, "Open at login", true, login, None);
        let menu = Menu::with_items(&[
            &status,
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(SHOW, "Show Overdesk", true, None),
            &disconnect,
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(UPDATES, "Check for updates", true, None),
            &login,
            &MenuItem::with_id(QUIT, "Quit Overdesk", true, None),
        ])
        .map_err(|e| e.to_string())?;
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
        let text = status_text(state);
        self.status.set_text(&text);
        self.disconnect
            .set_enabled(matches!(state.phase, Phase::Connected { .. }));
        let _ = self.icon.set_tooltip(Some(format!("Overdesk: {text}")));
    }
}

/// The status line for a state: "Not connected", "Connected to {peer}", "Using {peer}"
/// while driving, or "{peer} is using this Mac/PC" while driven.
fn status_text(state: &AppState) -> String {
    let Phase::Connected { peer, mode, .. } = &state.phase else {
        return "Not connected".into();
    };
    match mode {
        Mode::Local => format!("Connected to {peer}"),
        Mode::Driving(_) => format!("Using {peer}"),
        Mode::Driven(_) => {
            let device = if state.os == Os::MacOs { "Mac" } else { "PC" };
            format!("{peer} is using this {device}")
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
                UPDATES => Action::CheckUpdates,
                LOGIN => Action::ToggleLogin,
                QUIT => Action::Quit,
                _ => continue,
            };
            if out.send(action).await.is_err() {
                return;
            }
        }
    })
}
