// SPDX-License-Identifier: AGPL-3.0-only
//! The real OS adapter. macOS, Windows and Linux expose the same `Capture` / `Injector` API.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use overdesk_core::Rect;
use overdesk_core::engine::Action;
use overdesk_net::sync::Relock;
use overdesk_platform::input::{Captured, MoveFilter, Sink, Warnings};
#[cfg(free_unix)]
pub use overdesk_platform::linux::ffi::{
    Capture, Injector, main_display_size, monitors, permissions, request_permissions,
};
#[cfg(target_os = "macos")]
pub use overdesk_platform::mac::ffi::{
    Capture, Injector, main_display_size, monitors, permissions, request_permissions,
};
use overdesk_platform::text::{KeyChar, Typist};
#[cfg(windows)]
pub use overdesk_platform::win::ffi::{
    Capture, Injector, main_display_size, monitors, permissions, request_permissions,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::{Clipboard, Platform};

/// The OS keyboard-layout lookup behind [`Platform::key_text`].
#[cfg(target_os = "macos")]
type NativeLayout = overdesk_platform::mac::layout::MacLayout;
/// The OS keyboard-layout lookup behind [`Platform::key_text`].
#[cfg(windows)]
type NativeLayout = overdesk_platform::win::layout::WinLayout;
/// The OS keyboard-layout lookup behind [`Platform::key_text`].
#[cfg(free_unix)]
type NativeLayout = overdesk_platform::linux::layout::X11Layout;

/// The `FEATURE_*` bits this computer announces for gestures: a Mac says which Apple pointing
/// devices it has; every computer announces that it acts on a Mac's gestures (see `gestures`).
pub fn input_features() -> u64 {
    #[cfg(target_os = "macos")]
    return overdesk_platform::mac::ffi::pointing_features() | overdesk_proto::FEATURE_GESTURES;
    #[cfg(not(target_os = "macos"))]
    return overdesk_proto::FEATURE_GESTURES;
}

/// Why [`NativePlatform::start`] failed. The platform's own error is carried as its `Display` text.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The operating system has not allowed this app to see and control the keyboard and mouse.
    #[error("not allowed to see and control the keyboard and mouse")]
    PermissionDenied,
    /// Input capture could not start for another reason.
    #[error("input capture: {0}")]
    Capture(String),
    /// Input injection could not start for another reason.
    #[error("input injection: {0}")]
    Inject(String),
    /// The injector thread could not be spawned, or died before it was ready.
    #[error("injector thread: {0}")]
    Thread(String),
}

/// Whether a platform start error means a missing permission (only macOS asks for any).
#[cfg(target_os = "macos")]
fn denied(e: &overdesk_platform::mac::ffi::MacError) -> bool {
    use overdesk_platform::mac::ffi::MacError;
    matches!(e, MacError::NoListenPermission | MacError::NoPostPermission)
}

/// Whether a platform start error means a missing permission (only macOS asks for any).
#[cfg(not(target_os = "macos"))]
fn denied<E>(_: &E) -> bool {
    false
}

/// What the platform backends report, logged.
fn warnings() -> Warnings {
    Warnings::new(overdesk_proto::log::warn)
}

/// Captured input goes to whichever session asked for it last.
type Slot = Arc<Mutex<Option<UnboundedSender<Captured>>>>;

/// The [`Platform`] for macOS, Windows and Linux: real input capture and injection.
/// Cloning is cheap; every clone shares the same capture tap and injector thread.
#[derive(Clone)]
pub struct NativePlatform {
    /// The OS capture tap, shared so swallowing can be switched from any clone.
    capture: Arc<Capture>,
    /// Sends actions to the injector thread.
    inject: Sender<Action>,
    /// The session that currently receives captured input.
    slot: Slot,
    /// Which plain moves the capture thread may skip; see [`Platform::set_move_filter`].
    moves: Arc<MoveFilter>,
    /// Modifier and dead-key state of the character lookup, shared by every clone.
    typist: Arc<Mutex<Typist<NativeLayout>>>,
}

/// Empties `batch`, yielding its actions in order except that a run of consecutive
/// `InjectMove`s collapses to the last one (positions are absolute; any other action in
/// between keeps the moves on either side apart, so order against clicks is never changed).
fn coalesce_moves(batch: &mut Vec<Action>) -> impl Iterator<Item = Action> + '_ {
    let mut next = batch.drain(..).peekable();
    std::iter::from_fn(move || {
        let mut a = next.next()?;
        while matches!(a, Action::InjectMove { .. }) && matches!(next.peek(), Some(Action::InjectMove { .. })) {
            a = next.next()?;
        }
        Some(a)
    })
}

impl NativePlatform {
    /// Starts capture and the injector thread. Fails if the OS refuses either, for
    /// example when a permission is missing.
    pub fn start() -> Result<Self, StartError> {
        // The gesture choices saved by the app window; read once, then kept live by it.
        crate::gestures::set_live(crate::gestures::Settings::load(&crate::store_dir()).map);
        let slot: Slot = Arc::default();
        let current = slot.clone();
        // The capture thread hands each event straight to the current session's queue (a
        // tokio sender's `send` is sync and works from any thread): no bridge thread. With no
        // session, input is dropped. Always `Ok`: a session ending must not stop capture.
        let moves = Arc::new(MoveFilter::default());
        let sink = Sink::new(move |c| {
            if let Some(tx) = current.relock().as_ref() {
                let _ = tx.send(c);
            }
            Ok(())
        })
        .with_warnings(warnings())
        .with_move_filter(moves.clone());
        let capture = Capture::start_with(sink).map_err(|e| {
            if denied(&e) {
                StartError::PermissionDenied
            } else {
                StartError::Capture(e.to_string())
            }
        })?;
        let (inject, actions) = channel::<Action>();
        let (ready_tx, ready) = channel::<Result<(), StartError>>();
        // The injector (a Quartz event source on macOS) stays on one thread.
        std::thread::Builder::new()
            .name("overdesk-inject".into())
            .spawn(move || {
                #[cfg(free_unix)]
                let made = Injector::new(&crate::store_dir(), warnings());
                #[cfg(not(free_unix))]
                let made = Injector::new();
                let mut injector = match made {
                    Ok(i) => {
                        let _ = ready_tx.send(Ok(()));
                        i
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(if denied(&e) {
                            StartError::PermissionDenied
                        } else {
                            StartError::Inject(e.to_string())
                        }));
                        return;
                    }
                };
                let mut batch = Vec::new();
                while let Ok(first) = actions.recv() {
                    // Take everything queued: if the injector fell behind, only the newest
                    // of a run of moves matters (each is an absolute position).
                    batch.push(first);
                    batch.extend(actions.try_iter());
                    for a in coalesce_moves(&mut batch) {
                        match a {
                            Action::InjectMove { x, y } => injector.pointer(x as f64, y as f64),
                            Action::InjectButton { button, pressed } => injector.button(button, pressed),
                            Action::InjectKey { code, pressed } => {
                                injector.key(code, pressed);
                            }
                            Action::InjectScroll { dx, dy, unit, phase } => injector.scroll(dx, dy, unit, phase),
                            Action::InjectGesture { gesture, phase } => {
                                // Types the shortcut chosen for the gesture (a Mac's own); a pinch is replayed natively.
                                if let Some(chord) = crate::gestures::chord_for(gesture) {
                                    injector.chord(chord);
                                    continue;
                                }
                                injector.gesture(gesture, phase);
                            }
                            Action::ReleaseAll => injector.release_all(),
                            _ => {}
                        }
                    }
                }
            })
            .map_err(|e| StartError::Thread(e.to_string()))?;
        // Wait for the injector: start() fails rather than inject into nothing.
        ready.recv().map_err(|e| StartError::Thread(e.to_string()))??;
        Ok(Self {
            capture: Arc::new(capture),
            inject,
            slot,
            moves,
            typist: Arc::new(Mutex::new(Typist::new(NativeLayout::default()))),
        })
    }
}

impl Clipboard for NativePlatform {
    // A fresh handle per call: arboard's macOS handle is just NSPasteboard and its Windows
    // handle opens the clipboard only for the call, so nothing is cached or tied to the
    // main thread, and a clipboard another app is holding fails one call, not the session.
    // The loop in `run` checks privacy around this read, so it does not repeat it here.
    fn clipboard_text(&self) -> Option<String> {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }

    fn clipboard_is_private(&self) -> bool {
        overdesk_platform::clipboard_is_private()
    }

    fn set_clipboard_text(&self, text: &str) {
        if let Ok(mut c) = arboard::Clipboard::new() {
            let _ = c.set_text(text);
        }
    }

    fn clipboard_image(&self) -> Option<(u32, u32, Vec<u8>)> {
        if overdesk_platform::clipboard_is_private() {
            return None;
        }
        let img = arboard::Clipboard::new().ok()?.get_image().ok()?;
        Some((img.width as u32, img.height as u32, img.bytes.into_owned()))
    }

    fn set_clipboard_image(&self, width: u32, height: u32, rgba: Vec<u8>) {
        if let Ok(mut c) = arboard::Clipboard::new() {
            let _ = c.set_image(arboard::ImageData {
                width: width as usize,
                height: height as usize,
                bytes: rgba.into(),
            });
        }
    }

    fn clipboard_files(&self) -> Option<Vec<PathBuf>> {
        if overdesk_platform::clipboard_is_private() {
            return None;
        }
        arboard::Clipboard::new().ok()?.get().file_list().ok()
    }

    fn set_clipboard_files(&self, files: &[PathBuf]) {
        if let Ok(mut c) = arboard::Clipboard::new() {
            let _ = c.set().file_list(files);
        }
    }

    fn watch_clipboard(&self, on_change: Box<dyn Fn() + Send>) -> Option<Box<dyn Send>> {
        overdesk_platform::watch_clipboard(on_change).map(|w| Box::new(w) as Box<dyn Send>)
    }

    fn clipboard_change_count(&self) -> Option<i64> {
        overdesk_platform::clipboard_change_count()
    }
}

impl Platform for NativePlatform {
    fn swallow(&self, on: bool) {
        self.capture.set_swallow(on);
    }

    fn inject(&self, action: Action) {
        let _ = self.inject.send(action);
    }

    fn display_size(&self) -> Option<(u32, u32)> {
        Some(main_display_size())
    }

    fn monitors(&self) -> Vec<Rect> {
        monitors()
    }

    fn set_move_filter(&self, monitors: Option<&[Rect]>) {
        self.moves.set(monitors);
    }

    fn capture(&self) -> UnboundedReceiver<Captured> {
        let (tx, rx) = unbounded_channel();
        *self.slot.relock() = Some(tx);
        rx
    }

    fn key_text(&self, code: u32, pressed: bool, translate: bool) -> Option<KeyChar> {
        self.typist.relock().key(code, pressed, translate)
    }

    fn sync_key_state(&self) {
        self.typist.relock().sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(x: i32) -> Action {
        Action::InjectMove { x, y: 0 }
    }

    #[test]
    fn a_run_of_moves_keeps_only_the_last_and_clicks_keep_their_place() {
        let click = Action::InjectButton {
            button: overdesk_proto::Button::Left,
            pressed: true,
        };
        let mut batch = vec![mv(1), mv(2), click.clone(), mv(3), mv(4), mv(5), Action::ReleaseAll];
        let out: Vec<_> = coalesce_moves(&mut batch).collect();
        assert_eq!(out, vec![mv(2), click, mv(5), Action::ReleaseAll]);
        assert!(batch.is_empty());
        assert_eq!(coalesce_moves(&mut Vec::new()).count(), 0);
    }
}
