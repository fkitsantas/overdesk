// SPDX-License-Identifier: AGPL-3.0-only
//! Injection: `SendInput` for pointer, buttons, keys and scroll.

use overdesk_proto::{Button, Gesture, Phase, ScrollUnit};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};

use super::display::{cursor_local, virtual_screen};
use super::{INJECTED_TAG, WinError, XBUTTON1, XBUTTON2};
use crate::input::{CTRL_KEY, Pressed, Rect, pinch_wheel, to_units};
use crate::win::events::absolute;
use crate::win::keymap::code_to_scan;

/// Posts input that apps cannot tell apart from a real keyboard and mouse.
pub struct Injector {
    /// Overdesk codes this injector has pressed and not yet released.
    keys: Pressed<u32>,
    /// Buttons this injector has pressed and not yet released.
    buttons: Pressed<Button>,
}

/// Builds one `SendInput` mouse record tagged with [`INJECTED_TAG`]. `dx`/`dy` are
/// absolute (0..=65535) or relative depending on `flags`; `data` is the X button
/// number or the wheel delta.
fn mouse_input(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECTED_TAG,
            },
        },
    }
}

/// Injects `inputs` in one `SendInput` call, so Windows keeps them together and in
/// order. The return count is ignored: a blocked injection (for example into an
/// elevated window, under UIPI) cannot be retried usefully.
fn send_inputs(inputs: &[INPUT]) {
    // SAFETY: `inputs` is a valid slice of INPUT structs for the duration of the call.
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
}

impl Injector {
    /// Creates an injector for the virtual screen. Never fails on Windows; the
    /// `Result` matches the macOS adapter.
    pub fn new() -> Result<Self, WinError> {
        Ok(Self {
            keys: Pressed::default(),
            buttons: Pressed::default(),
        })
    }

    /// Where the system cursor is now (virtual-screen pixels).
    pub fn cursor() -> (f64, f64) {
        let (x, y) = cursor_local();
        (f64::from(x), f64::from(y))
    }

    /// Moves the pointer to virtual-screen pixel coordinates. `MOUSEEVENTF_VIRTUALDESK`
    /// maps 0..=65535 across the whole virtual screen, so box-relative input needs no origin.
    pub fn pointer(&mut self, x: f64, y: f64) {
        let Rect { w, h, .. } = virtual_screen(); // live: survives monitor hot-plug
        let (ax, ay) = absolute(x as i32, y as i32, w, h);
        send_inputs(&[mouse_input(
            ax,
            ay,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )]);
    }

    /// Presses or releases `button` at the current cursor position, tracking it so
    /// [`Injector::release_all`] can release it later.
    pub fn button(&mut self, button: Button, pressed: bool) {
        let (flags, data) = match (button, pressed) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Back, p) => (if p { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1),
            (Button::Forward, p) => (if p { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2),
        };
        self.buttons.set(button, pressed);
        // Windows counts double-clicks itself from timing and position.
        send_inputs(&[mouse_input(0, 0, data, flags)]);
    }

    /// Returns `false` for codes Windows has no key for (e.g. Fn/Globe).
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        let Some(scan) = code_to_scan(code) else { return false };
        self.keys.set(code, pressed);
        // Inject the scan code rather than a virtual key, so the active layout maps it
        // exactly as it would a physical key; the `0xE0` prefix becomes a flag.
        let mut flags = KEYEVENTF_SCANCODE;
        if scan & 0xE000 == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan & 0xFF,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_TAG,
                },
            },
        };
        send_inputs(&[input]);
        true
    }

    /// Windows has no scroll phases; the Mac's momentum events already carry the inertia.
    pub fn scroll(&mut self, dx: i32, dy: i32, unit: ScrollUnit, _phase: Phase) {
        let mut inputs = Vec::with_capacity(2);
        if dy != 0 {
            // `mouseData` is a DWORD, but Windows reads the wheel delta as signed.
            inputs.push(mouse_input(0, 0, to_units(dy, unit) as u32, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            inputs.push(mouse_input(0, 0, to_units(dx, unit) as u32, MOUSEEVENTF_HWHEEL));
        }
        send_inputs(&inputs);
    }

    /// Replays a pinch as Ctrl+mouse wheel, the zoom shortcut of browsers, editors and
    /// Office. Rotate and smart zoom have no public Windows equivalent and are ignored.
    pub fn gesture(&mut self, gesture: Gesture, phase: Phase) {
        let Some(wheel) = pinch_wheel(gesture) else { return };
        // Ctrl stays held only for this wheel, unless the remote user already holds it.
        let held = self.keys.contains(&CTRL_KEY);
        if !held {
            self.key(CTRL_KEY, true);
        }
        self.scroll(0, wheel, ScrollUnit::Notch120, phase);
        if !held {
            self.key(CTRL_KEY, false);
        }
    }

    /// Releases every key and button this injector holds down.
    pub fn release_all(&mut self) {
        for code in self.keys.snapshot().into_iter().rev() {
            self.key(code, false);
        }
        for button in self.buttons.snapshot() {
            self.button(button, false);
        }
    }
}

impl Drop for Injector {
    /// Nothing stays held down once the injector goes away.
    fn drop(&mut self) {
        self.release_all();
    }
}
