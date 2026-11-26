// SPDX-License-Identifier: AGPL-3.0-only
//! Win32 capture (low-level hooks + Raw Input) and injection (`SendInput`). Same API as
//! the macOS adapter. Runs as a standard user: no service, no admin (ARCHITECTURE §8).
#![allow(unsafe_code)] // FFI boundary; each unsafe block states its invariant.

use std::cell::RefCell;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use overdesk_proto::{Button, Phase, ScrollUnit};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE,
    RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    HC_ACTION, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_INPUT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
};

use super::events::{absolute, wheel_units};
use super::keymap::{code_to_scan, scan_to_code};
pub use crate::input::Captured;
use crate::input::EscapeChord;
use crate::mac::events::{Held, LocalDowns};

/// Put in `dwExtraInfo` of everything we inject, so our own hooks skip it. Every peer
/// both captures and injects; without this, input would loop forever.
const INJECTED_TAG: usize = 0x4F56_4552; // "OVER"
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;
/// Raw Input `usFlags`: absolute pointing devices (pens, RDP) have no deltas.
const MOUSE_MOVE_ABSOLUTE: u16 = 1;

#[derive(Debug, PartialEq, Eq)]
pub enum WinError {
    HookFailed,
    WindowFailed,
    RawInputFailed,
}

/// Coordinates in real pixels, not DPI-virtualised ones. Process-wide, set once.
fn ensure_dpi_aware() {
    static ONCE: Once = Once::new();
    // SAFETY: plain Win32 call with a documented constant.
    ONCE.call_once(|| unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

/// Primary display size in pixels.
/// shortcut: primary monitor only; multi-monitor PCs need per-monitor screens on the Desk.
pub fn main_display_size() -> (u32, u32) {
    ensure_dpi_aware();
    // SAFETY: plain Win32 calls.
    unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN) as u32,
            GetSystemMetrics(SM_CYSCREEN) as u32,
        )
    }
}

/// Windows needs no permission prompt for hooks or `SendInput` as a standard user.
pub fn permissions() -> (bool, bool) {
    (true, true)
}

pub fn request_permissions() {}

struct HookCtx {
    tx: Sender<Captured>,
    swallow: Arc<AtomicBool>,
    local: LocalDowns,
    chord: EscapeChord,
}

thread_local! {
    // Hooks and the Raw Input window procedure all run on the capture thread.
    static CTX: RefCell<Option<HookCtx>> = const { RefCell::new(None) };
}

fn send(ctx: &HookCtx, c: Captured) {
    // Nobody listening any more (engine gone): never keep swallowing input.
    if ctx.tx.send(c).is_err() {
        ctx.swallow.store(false, Ordering::Relaxed);
    }
}

/// Decides delivery for a press/release edge, with the escape chord checked first.
fn edge(ctx: &mut HookCtx, held: Held, down: bool) -> bool {
    if let Held::Key(code) = held
        && ctx.chord.key(code, down)
    {
        ctx.swallow.store(false, Ordering::Relaxed);
        send(ctx, Captured::Escape);
    }
    let swallowing = ctx.swallow.load(Ordering::Relaxed);
    ctx.local.deliver_locally(held, down, swallowing)
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT valid for this call.
    let pass = || unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    if code != HC_ACTION as i32 {
        return pass();
    }
    let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    if k.dwExtraInfo == INJECTED_TAG {
        return pass();
    }
    let base = k.scanCode as u16 & 0xFF;
    let scan = if k.flags & LLKHF_EXTENDED != 0 {
        0xE000 | base
    } else {
        base
    };
    // Some keys report an extended flag inconsistently (e.g. Num Lock): fall back.
    let Some(code_) = scan_to_code(scan).or_else(|| scan_to_code(base)) else {
        return pass();
    };
    let down = k.flags & LLKHF_UP == 0;
    let deliver = CTX.with_borrow_mut(|c| {
        c.as_mut().is_none_or(|ctx| {
            send(
                ctx,
                Captured::Key {
                    code: code_,
                    pressed: down,
                },
            );
            edge(ctx, Held::Key(code_), down)
        })
    });
    if deliver { pass() } else { 1 }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: for HC_ACTION, lparam points to an MSLLHOOKSTRUCT valid for this call.
    let pass = || unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    if code != HC_ACTION as i32 {
        return pass();
    }
    let m = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
    if m.dwExtraInfo == INJECTED_TAG {
        return pass();
    }
    let hi = (m.mouseData >> 16) as u16;
    let button = |b, down| {
        Some((
            Held::Button(b),
            down,
            Captured::Button {
                button: b,
                pressed: down,
            },
        ))
    };
    let x_button = |down| {
        button(
            if hi as u32 == XBUTTON2 {
                Button::Forward
            } else {
                Button::Back
            },
            down,
        )
    };
    let ev = match wparam as u32 {
        WM_LBUTTONDOWN => button(Button::Left, true),
        WM_LBUTTONUP => button(Button::Left, false),
        WM_RBUTTONDOWN => button(Button::Right, true),
        WM_RBUTTONUP => button(Button::Right, false),
        WM_MBUTTONDOWN => button(Button::Middle, true),
        WM_MBUTTONUP => button(Button::Middle, false),
        WM_XBUTTONDOWN => x_button(true),
        WM_XBUTTONUP => x_button(false),
        _ => None,
    };
    let deliver = CTX.with_borrow_mut(|c| {
        let Some(ctx) = c.as_mut() else { return true };
        match (wparam as u32, ev) {
            (_, Some((held, down, captured))) => {
                send(ctx, captured);
                edge(ctx, held, down)
            }
            (WM_MOUSEWHEEL | WM_MOUSEHWHEEL, None) => {
                let delta = hi as i16 as i32;
                let (dx, dy) = if wparam as u32 == WM_MOUSEWHEEL {
                    (0, delta)
                } else {
                    (delta, 0)
                };
                send(
                    ctx,
                    Captured::Scroll {
                        dx,
                        dy,
                        unit: ScrollUnit::Notch120,
                        phase: Phase::None,
                    },
                );
                !ctx.swallow.load(Ordering::Relaxed)
            }
            // Motion deltas come from Raw Input; here we only decide whether the cursor
            // may move. Blocking it in this hook keeps it put while driving a peer.
            (WM_MOUSEMOVE, None) => !ctx.swallow.load(Ordering::Relaxed),
            _ => true,
        }
    });
    if deliver { pass() } else { 1 }
}

unsafe extern "system" fn raw_input_window(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        // SAFETY: lparam is the HRAWINPUT of this WM_INPUT; `raw` has `size` bytes.
        let got = unsafe {
            GetRawInputData(
                lparam as HRAWINPUT,
                RID_INPUT,
                (&mut raw as *mut RAWINPUT).cast(),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        // hDevice 0 means injected (SendInput): never ours to capture.
        if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE && !raw.header.hDevice.is_null() {
            // SAFETY: dwType is RIM_TYPEMOUSE, so the mouse arm of the union is active.
            let mouse = unsafe { raw.data.mouse };
            let (dx, dy) = (mouse.lLastX, mouse.lLastY);
            if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 && (dx, dy) != (0, 0) {
                let mut at = POINT { x: 0, y: 0 };
                // SAFETY: writes one POINT.
                unsafe { GetCursorPos(&mut at) };
                CTX.with_borrow(|c| {
                    if let Some(ctx) = c {
                        // shortcut: raw deltas skip Windows pointer acceleration; apply the
                        // user's pointer-speed curve if driving a peer feels slow.
                        send(
                            ctx,
                            Captured::Moved {
                                dx,
                                dy,
                                x: at.x,
                                y: at.y,
                            },
                        );
                    }
                });
            }
        }
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A running capture thread. Dropping it unhooks and stops the thread.
pub struct Capture {
    swallow: Arc<AtomicBool>,
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    pub fn start() -> Result<(Self, Receiver<Captured>), WinError> {
        ensure_dpi_aware();
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let swallow = Arc::new(AtomicBool::new(false));
        let swallow_t = swallow.clone();
        let thread = std::thread::Builder::new()
            .name("overdesk-win-capture".into())
            .spawn(move || run_capture(tx, swallow_t, ready_tx))
            .map_err(|_| WinError::HookFailed)?;
        let thread_id = ready_rx.recv().map_err(|_| WinError::HookFailed)??;
        Ok((
            Self {
                swallow,
                thread_id,
                thread: Some(thread),
            },
            rx,
        ))
    }

    /// While on, input is consumed instead of reaching local apps, and the cursor stays put.
    pub fn set_swallow(&self, on: bool) {
        self.swallow.store(on, Ordering::Relaxed);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: posting WM_QUIT to our own capture thread ends its message loop.
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn run_capture(tx: Sender<Captured>, swallow: Arc<AtomicBool>, ready: Sender<Result<u32, WinError>>) {
    CTX.with_borrow_mut(|c| {
        *c = Some(HookCtx {
            tx,
            swallow,
            local: LocalDowns::default(),
            chord: EscapeChord::default(),
        })
    });
    let class = wide("OverdeskRawInput");
    // SAFETY: Win32 setup on this thread; the class name outlives the window.
    let (kb, mouse) = unsafe {
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(raw_input_window),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err(WinError::WindowFailed));
            return;
        }
        let device = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        if RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
            let _ = ready.send(Err(WinError::RawInputFailed));
            return;
        }
        let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0);
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), instance, 0);
        if kb.is_null() || mouse.is_null() {
            let _ = ready.send(Err(WinError::HookFailed));
            return;
        }
        (kb, mouse)
    };
    // SAFETY: plain Win32 call.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    // Low-level hooks are called through this thread's message loop.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: standard message loop on the thread that owns the hooks and window.
    unsafe {
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(kb);
        UnhookWindowsHookEx(mouse);
    }
    CTX.with_borrow_mut(|c| *c = None);
}

/// Posts input that apps cannot tell apart from a real keyboard and mouse.
pub struct Injector {
    keys: Vec<u32>,
    buttons: Vec<Button>,
    size: (u32, u32),
}

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
    pub fn new() -> Result<Self, WinError> {
        Ok(Self {
            keys: Vec::new(),
            buttons: Vec::new(),
            size: main_display_size(),
        })
    }

    /// Where the system cursor is now (pixels).
    pub fn cursor() -> (f64, f64) {
        let mut at = POINT { x: 0, y: 0 };
        // SAFETY: writes one POINT.
        unsafe { GetCursorPos(&mut at) };
        (at.x as f64, at.y as f64)
    }

    /// Moves the pointer to primary-screen pixel coordinates.
    pub fn pointer(&mut self, x: f64, y: f64) {
        let (ax, ay) = absolute(x as i32, y as i32, self.size.0, self.size.1);
        send_inputs(&[mouse_input(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)]);
    }

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
        self.buttons.retain(|b| *b != button);
        if pressed {
            self.buttons.push(button);
        }
        // Windows counts double-clicks itself from timing and position.
        send_inputs(&[mouse_input(0, 0, data, flags)]);
    }

    /// Returns `false` for codes Windows has no key for (e.g. Fn/Globe).
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        let Some(scan) = code_to_scan(code) else { return false };
        self.keys.retain(|k| *k != code);
        if pressed {
            self.keys.push(code);
        }
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
            inputs.push(mouse_input(0, 0, wheel_units(dy, unit) as u32, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            inputs.push(mouse_input(0, 0, wheel_units(dx, unit) as u32, MOUSEEVENTF_HWHEEL));
        }
        send_inputs(&inputs);
    }

    /// Releases every key and button this injector holds down.
    pub fn release_all(&mut self) {
        for code in self.keys.clone().into_iter().rev() {
            self.key(code, false);
        }
        for button in self.buttons.clone() {
            self.button(button, false);
        }
    }
}
