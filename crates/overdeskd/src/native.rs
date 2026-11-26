// SPDX-License-Identifier: AGPL-3.0-only
//! The real OS adapter. macOS and Windows expose the same `Capture` / `Injector` API.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use overdesk_core::engine::Action;
use overdesk_platform::input::Captured;
#[cfg(target_os = "macos")]
pub use overdesk_platform::mac::ffi::{Capture, Injector, main_display_size, permissions, request_permissions};
#[cfg(windows)]
pub use overdesk_platform::win::ffi::{Capture, Injector, main_display_size, permissions, request_permissions};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::Platform;

/// Captured input goes to whichever session asked for it last.
type Slot = Arc<Mutex<Option<UnboundedSender<Captured>>>>;

#[derive(Clone)]
pub struct NativePlatform {
    capture: Arc<Capture>,
    inject: Sender<Action>,
    slot: Slot,
}

impl NativePlatform {
    pub fn start() -> Result<Self, String> {
        let (capture, events) = Capture::start().map_err(|e| format!("capture: {e:?}"))?;
        let slot: Slot = Arc::default();
        let bridge = slot.clone();
        std::thread::Builder::new()
            .name("overdesk-capture-bridge".into())
            .spawn(move || {
                while let Ok(c) = events.recv() {
                    if let Some(tx) = bridge.lock().expect("slot lock").as_ref() {
                        let _ = tx.send(c);
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let (inject, actions) = channel::<Action>();
        let (ready_tx, ready) = channel::<Result<(), String>>();
        // The injector (a Quartz event source on macOS) stays on one thread.
        std::thread::Builder::new()
            .name("overdesk-inject".into())
            .spawn(move || {
                let mut injector = match Injector::new() {
                    Ok(i) => {
                        let _ = ready_tx.send(Ok(()));
                        i
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("inject: {e:?}")));
                        return;
                    }
                };
                while let Ok(a) = actions.recv() {
                    match a {
                        Action::InjectMove { x, y } => injector.pointer(x as f64, y as f64),
                        Action::InjectButton { button, pressed } => injector.button(button, pressed),
                        Action::InjectKey { code, pressed } => {
                            injector.key(code, pressed);
                        }
                        Action::InjectScroll { dx, dy, unit, phase } => injector.scroll(dx, dy, unit, phase),
                        Action::ReleaseAll => injector.release_all(),
                        _ => {}
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        ready.recv().map_err(|e| e.to_string())??;
        Ok(Self {
            capture: Arc::new(capture),
            inject,
            slot,
        })
    }
}

impl Platform for NativePlatform {
    fn swallow(&self, on: bool) {
        self.capture.set_swallow(on);
    }

    fn inject(&self, action: Action) {
        let _ = self.inject.send(action);
    }

    fn capture(&self) -> UnboundedReceiver<Captured> {
        let (tx, rx) = unbounded_channel();
        *self.slot.lock().expect("slot lock") = Some(tx);
        rx
    }
}
