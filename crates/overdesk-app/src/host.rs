// SPDX-License-Identifier: AGPL-3.0-only
//! The app's place in the system: the tray icon, the "Open at login" item, closing the window
//! and what the build's [`Extension`](crate::Extension) asks of the app.

use iced::Task;
use overdeskd::controller::{Command, Phase};

use super::ext::Request;
use super::{
    App, EDITION, EditionReport, Message, SHUTDOWN_GRACE, Screen, background, diag, free_report, login, store_dir, tray,
};

impl App {
    /// Applies the messages about the app's place in the system: the tray, the login item,
    /// closing the window and the extension's requests; any other message does nothing.
    pub(super) fn apply_host(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::CreateTray => self.create_tray(),
            Message::CloseRequested(id) => {
                // With no tray there would be no way back, so closing must quit then.
                if self.tray.is_some() {
                    self.visible = false;
                    iced::window::set_mode(id, iced::window::Mode::Hidden)
                } else {
                    iced::exit()
                }
            }
            Message::Tray(tray::Action::Show) => {
                self.visible = true;
                iced::window::oldest().and_then(|id| {
                    Task::batch([
                        iced::window::set_mode(id, iced::window::Mode::Windowed),
                        iced::window::gain_focus(id),
                    ])
                })
            }
            Message::Tray(tray::Action::Menu(id)) => {
                if let Some(ext) = &self.ext {
                    ext.on_menu(&id);
                }
                Task::none()
            }
            Message::ExtensionButton => {
                if let Some(ext) = &self.ext {
                    ext.on_button();
                }
                Task::none()
            }
            Message::Host(request) => self.host_request(request),
            Message::Tray(tray::Action::Report) => self.diagnose(true),
            Message::Tray(tray::Action::SaveDiagnostics) => self.diagnose(false),
            Message::Diagnosed(result) => {
                self.notice = Some(match result {
                    Ok(saved) => saved.notice(),
                    Err(e) => {
                        overdesk_proto::log::warn(format_args!("diagnostics not saved: {e}"));
                        "Couldn't save the diagnostics file. Check that Downloads is writable, then try again.".into()
                    }
                });
                // The notice is in the window, which may be hidden in the tray.
                Task::done(Message::Tray(tray::Action::Show))
            }
            Message::ToggleLogin | Message::Tray(tray::Action::ToggleLogin) => {
                self.toggle_login();
                Task::none()
            }
            Message::Tray(tray::Action::Disconnect) => {
                self.send(Command::Disconnect);
                Task::none()
            }
            Message::Tray(tray::Action::Quit) => {
                self.send(Command::Disconnect);
                // Give the controller a moment to release input before the process ends.
                Task::perform(tokio::time::sleep(SHUTDOWN_GRACE), |()| ())
                    .discard()
                    .chain(iced::exit())
            }
            // `App::apply` sends here only what it matches above.
            _ => Task::none(),
        }
    }

    /// Saves the diagnostics file off the UI thread, then opens the support page (`report`) or the
    /// file manager, and reports back as [`Message::Diagnosed`].
    fn diagnose(&self, report: bool) -> Task<Message> {
        let (state, links) = match &self.screen {
            Screen::Running { state, .. } => (
                Some(state),
                match &state.phase {
                    Phase::Connected { peers, .. } => peers.len(),
                    _ => 0,
                },
            ),
            _ => (None, 0),
        };
        let edition = self.edition_report(links);
        let facts = diag::facts(state, edition);
        let store = store_dir();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || diag::run(facts, &store, report))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            },
            Message::Diagnosed,
        )
    }

    /// What the build says about its edition now, `links` computers being linked.
    fn edition_report(&self, links: usize) -> EditionReport {
        EDITION.get().map_or_else(|| free_report(links), |e| (e.report)(links))
    }

    /// Creates the tray icon, or copes without it.
    fn create_tray(&mut self) -> Task<Message> {
        let state = match &self.screen {
            Screen::Running { state, .. } => Some(state),
            _ => None,
        };
        match tray::Tray::new(
            state,
            self.login,
            &self.ext.as_ref().map(|e| e.menu()).unwrap_or_default(),
        ) {
            Ok(tray) => self.tray = Some(tray),
            // Without a tray the window still works; closing it simply quits no more.
            Err(e) => {
                overdesk_proto::log::warn(format_args!("tray icon unavailable: {e}"));
                // Started hidden with no tray to bring it back: show it, or the app is invisible.
                if background() {
                    return Task::done(Message::Tray(tray::Action::Show));
                }
            }
        }
        Task::none()
    }

    /// Takes what the extension asked for.
    fn host_request(&mut self, request: Request) -> Task<Message> {
        match request {
            Request::Status(line) => self.status = line,
            Request::Notice(text) => self.notice = Some(text),
            Request::ShowWindow => return Task::done(Message::Tray(tray::Action::Show)),
            Request::Restart => {
                self.send(Command::Disconnect);
                // Let the controller send its goodbye before the process is replaced.
                return Task::perform(tokio::time::sleep(SHUTDOWN_GRACE), |()| Message::Relaunch);
            }
        }
        Task::none()
    }

    /// Flips "Open at login" and shows what really happened.
    fn toggle_login(&mut self) {
        match login::set(!self.login) {
            Ok(()) => {
                self.login = !self.login;
                self.login_error = None;
            }
            Err(e) => {
                overdesk_proto::log::warn(format_args!("open at login not changed: {e}"));
                self.login_error =
                    Some("Couldn't change that. Overdesk isn't allowed to edit your login items.".into());
            }
        }
        // The menu flips its own tick on click; make it match what really happened.
        if let Some(tray) = &self.tray {
            tray.set_login(self.login);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext::{Extension, StatusLine};
    use overdeskd::controller::AppState;
    use std::sync::{Arc, Mutex};

    /// An extension that records what the app tells it.
    #[derive(Default)]
    struct Fake(Mutex<Vec<String>>);

    impl Extension for Fake {
        fn on_menu(&self, id: &str) {
            self.0.lock().unwrap().push(format!("menu:{id}"));
        }
        fn on_button(&self) {
            self.0.lock().unwrap().push("button".into());
        }
        fn on_state(&self, _: &AppState) {
            self.0.lock().unwrap().push("state".into());
        }
    }

    /// An app wired to a fake extension.
    fn app() -> (App, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        let mut app = App::boot().0;
        app.ext = Some(fake.clone());
        (app, fake)
    }

    #[test]
    fn a_menu_click_reaches_the_extension() {
        let (mut app, fake) = app();
        let _ = app.apply(Message::Tray(tray::Action::Menu("x".into())));
        assert_eq!(*fake.0.lock().unwrap(), ["menu:x"]);
    }

    #[test]
    fn the_status_line_follows_the_extension_and_its_button_reaches_it() {
        let (mut app, fake) = app();
        let line = StatusLine {
            text: "Ready".into(),
            button: Some(("Go".into(), true)),
        };
        let _ = app.apply(Message::Host(Request::Status(Some(line.clone()))));
        assert_eq!(app.status, Some(line));
        let _ = app.apply(Message::ExtensionButton);
        assert_eq!(*fake.0.lock().unwrap(), ["button"]);
        let _ = app.apply(Message::Host(Request::Status(None)));
        assert_eq!(app.status, None);
    }

    #[test]
    fn a_notice_from_the_extension_is_shown() {
        let (mut app, _) = app();
        let _ = app.apply(Message::Host(Request::Notice("Hello".into())));
        assert_eq!(app.notice.as_deref(), Some("Hello"));
    }

    #[test]
    fn a_saved_file_is_announced_and_a_failure_hides_the_raw_error() {
        let (mut app, _) = app();
        let saved = diag::Saved {
            name: "d.txt".into(),
            path: "/Users/x/Downloads/d.txt".into(),
            in_downloads: true,
            opened: true,
            url: None,
        };
        let _ = app.apply(Message::Diagnosed(Ok(saved)));
        assert_eq!(
            app.notice.as_deref(),
            Some("Saved d.txt to Downloads. Attach it to your report.")
        );
        let _ = app.apply(Message::Diagnosed(Err("os error 13 at /secret/path".into())));
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.contains("Couldn't save") && !n.contains("secret"))
        );
    }

    #[test]
    fn the_extension_sees_every_state() {
        let (mut app, fake) = app();
        let state = AppState::new("Me".into(), overdesk_proto::Os::MacOs);
        let _ = app.apply(Message::State(state));
        assert_eq!(*fake.0.lock().unwrap(), ["state"]);
    }
}
