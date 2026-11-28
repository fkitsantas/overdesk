// SPDX-License-Identifier: AGPL-3.0-only
//! Capture: the low-level keyboard and mouse hooks, Raw Input for unaccelerated motion, and the
//! thread whose message loop runs them.

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI8, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use overdesk_proto::{Button, Phase, ScrollUnit};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
use windows_sys::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DefWindowProcW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSLLHOOKSTRUCT,
    SPI_GETMOUSE, SPI_GETMOUSESPEED, SetTimer, SetWindowsHookExW, SystemParametersInfoW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_DISPLAYCHANGE, WM_INPUT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WM_XBUTTONDOWN,
    WM_XBUTTONUP,
};

use super::display::{VIRTUAL, cursor_local, ensure_dpi_aware};
use super::window::{message_window, pump, quit_thread};
use super::{Captured, INJECTED_TAG, WinError, XBUTTON2, wide};
use crate::input::{CAPS_CODE, EscapeChord, Held, LocalDowns, Sink};
use crate::win::events::{Ballistics, CapsTracker, FAKE_LCTRL_SCAN, hook_is_dead, passes_unforwarded_key};
use crate::win::keymap::scan_to_code;

/// Raw Input `usFlags`: absolute pointing devices (pens, RDP) have no deltas.
const MOUSE_MOVE_ABSOLUTE: u16 = 1;

/// State shared by the hook procedures and the Raw Input window procedure. Win32 hook
/// callbacks take no user pointer, so it lives in the capture thread's [`CTX`].
struct HookCtx {
    /// Where captured input goes; the receiving end belongs to the engine.
    tx: Sink,
    /// Set while focus is on another peer, so local input is consumed. Shared with
    /// [`Capture::set_swallow`].
    swallow: Arc<AtomicBool>,
    /// Keeps local apps' press/release pairs balanced across swallow changes.
    local: LocalDowns,
    /// Failsafe detector for the escape chord.
    chord: EscapeChord,
    /// The user's pointer-speed and acceleration settings, read when capture starts.
    ballistics: Ballistics,
    /// When the previous mouse report arrived, for the speed the curve is a function of.
    last_move: Instant,
    /// Caps Lock as the keys that reached the OS leave it; see [`caps_lock_on`].
    caps: CapsTracker,
}

/// Caps Lock as tracked by the capture thread: 1 on, 0 off, -1 not capturing (unknown).
static CAPS: AtomicI8 = AtomicI8::new(-1);

/// Whether Caps Lock is on. While capture runs this is the tracker the hook thread keeps,
/// because `GetKeyState` from a thread with no message queue (the tokio workers that ask)
/// returns a stale snapshot; without capture it is a plain `GetKeyState`, the best there is.
/// The tracker follows the keys that reached the OS, so a toggle made by other means
/// (an on-screen keyboard, the peer injecting Caps) is picked up only at the next restart.
pub fn caps_lock_on() -> bool {
    match CAPS.load(Ordering::Relaxed) {
        // SAFETY: a plain query; bit 0 is the toggle state.
        -1 => (unsafe { GetKeyState(0x14) } & 1) != 0,
        v => v == 1,
    }
}

/// Whether the swallow flag is set, from a hook; `false` when capture is shutting down.
fn swallowing() -> bool {
    CTX.with_borrow(|c| c.as_ref().is_some_and(|ctx| ctx.swallow.load(Ordering::Relaxed)))
}

thread_local! {
    /// The capture thread's hook state; `None` on every other thread and after
    /// capture ends.
    // Hooks and the Raw Input window procedure all run on the capture thread.
    static CTX: RefCell<Option<HookCtx>> = const { RefCell::new(None) };
}

/// When a device's low-level hook last ran and its Raw Input last reported, to spot a hook
/// Windows has removed (see [`hook_is_dead`]). Capture thread only.
struct HookClock {
    /// The hook's last call, or its installation if it has not run since.
    hook: Cell<Instant>,
    /// The last report from a physical device, `None` until one arrives.
    raw: Cell<Option<Instant>>,
}

impl HookClock {
    /// A clock for a hook installed now.
    fn new() -> Self {
        Self {
            hook: Cell::new(Instant::now()),
            raw: Cell::new(None),
        }
    }
}

thread_local! {
    /// The keyboard hook's clock.
    static KB_CLOCK: HookClock = HookClock::new();
    /// The mouse hook's clock.
    static MOUSE_CLOCK: HookClock = HookClock::new();
}

/// Sends one captured event to the engine, turning swallowing off if the engine has
/// gone so local input can never be lost.
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

/// `WH_KEYBOARD_LL` hook procedure. Forwards every physical key edge and returns a
/// non-zero value to consume the key while swallowing; injected keys (tagged with
/// [`INJECTED_TAG`]) and non-`HC_ACTION` calls go straight to the next hook.
///
/// # Safety
///
/// Called only by Windows on the capture thread, with `lparam` pointing to a valid
/// `KBDLLHOOKSTRUCT` whenever `code` is `HC_ACTION`.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic must not unwind into Windows; on one, the key goes to the next hook.
    // SAFETY: same contract as this function, passed straight through.
    catch_unwind(AssertUnwindSafe(|| unsafe { keyboard_event(code, wparam, lparam) }))
        // SAFETY: calling the next hook with the arguments Windows gave us is always allowed.
        .unwrap_or_else(|_| unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) })
}

/// The body of [`keyboard_hook`].
///
/// # Safety
///
/// As for [`keyboard_hook`].
unsafe fn keyboard_event(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    KB_CLOCK.with(|c| c.hook.set(Instant::now()));
    // SAFETY: calling the next hook with the arguments Windows gave us is always allowed.
    let pass = || unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    if code != HC_ACTION as i32 {
        return pass();
    }
    // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT valid for this call.
    let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
    if k.dwExtraInfo == INJECTED_TAG {
        return pass();
    }
    // The Ctrl Windows injects ahead of AltGr is not a key press: never forward or track it.
    if k.scanCode == FAKE_LCTRL_SCAN {
        let up = k.flags & LLKHF_UP != 0;
        return if passes_unforwarded_key(!up, swallowing()) {
            pass()
        } else {
            1
        };
    }
    // Rebuild the `0xE0nn` form the keymap uses: the hook reports the extended prefix
    // as a flag instead of in the scan code.
    let base = k.scanCode as u16 & 0xFF;
    let scan = if k.flags & LLKHF_EXTENDED != 0 {
        0xE000 | base
    } else {
        base
    };
    // Some keys report an extended flag inconsistently (e.g. Num Lock): fall back.
    let down = k.flags & LLKHF_UP == 0;
    let Some(code_) = scan_to_code(scan).or_else(|| scan_to_code(base)) else {
        // Not a key Overdesk knows: it must not leak to this PC while the peer is driven,
        // but its release always passes so it cannot stick.
        return if passes_unforwarded_key(down, swallowing()) {
            pass()
        } else {
            1
        };
    };
    // No context means capture is shutting down: always let the key through.
    let deliver = CTX.with_borrow_mut(|c| {
        c.as_mut().is_none_or(|ctx| {
            send(
                ctx,
                Captured::Key {
                    code: code_,
                    pressed: down,
                },
            );
            let deliver = edge(ctx, Held::Key(code_), down);
            if code_ == CAPS_CODE {
                CAPS.store(i8::from(ctx.caps.feed(down, deliver)), Ordering::Relaxed);
            }
            deliver
        })
    });
    // A non-zero return from a low-level hook discards the event system-wide.
    if deliver { pass() } else { 1 }
}

/// `WH_MOUSE_LL` hook procedure. Forwards buttons and wheel steps, and consumes them
/// and cursor moves while swallowing. Motion deltas are not taken from here: they come
/// from Raw Input in [`raw_input_window`].
///
/// # Safety
///
/// Called only by Windows on the capture thread, with `lparam` pointing to a valid
/// `MSLLHOOKSTRUCT` whenever `code` is `HC_ACTION`.
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic must not unwind into Windows; on one, the event goes to the next hook.
    // SAFETY: same contract as this function, passed straight through.
    catch_unwind(AssertUnwindSafe(|| unsafe { mouse_event(code, wparam, lparam) }))
        // SAFETY: calling the next hook with the arguments Windows gave us is always allowed.
        .unwrap_or_else(|_| unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) })
}

/// The body of [`mouse_hook`].
///
/// # Safety
///
/// As for [`mouse_hook`].
unsafe fn mouse_event(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    MOUSE_CLOCK.with(|c| c.hook.set(Instant::now()));
    // SAFETY: calling the next hook with the arguments Windows gave us is always allowed.
    let pass = || unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) };
    if code != HC_ACTION as i32 {
        return pass();
    }
    // SAFETY: for HC_ACTION, lparam points to an MSLLHOOKSTRUCT valid for this call.
    let m = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
    if m.dwExtraInfo == INJECTED_TAG {
        return pass();
    }
    // The high word of `mouseData` is the X button number for X button messages and
    // the signed wheel delta (multiples of `WHEEL_DELTA`, 120) for wheel messages.
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

/// Window procedure of the hidden message-only window registered for Raw Input. Turns
/// `WM_INPUT` mouse reports into [`Captured::Moved`] with unaccelerated device deltas,
/// which keep flowing even when the cursor is pinned or at a screen edge.
///
/// # Safety
///
/// Called only by Windows, on the capture thread that created the window, with
/// message parameters matching `msg`.
unsafe extern "system" fn raw_input_window(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic must not unwind into Windows; on one, the message gets default handling.
    // SAFETY: same contract as this function, passed straight through.
    catch_unwind(AssertUnwindSafe(|| unsafe {
        raw_input_message(hwnd, msg, wparam, lparam)
    }))
    // SAFETY: default handling with the arguments Windows gave us is always allowed.
    .unwrap_or_else(|_| unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
}

/// The body of [`raw_input_window`].
///
/// # Safety
///
/// As for [`raw_input_window`].
unsafe fn raw_input_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DISPLAYCHANGE
        && let Ok(mut c) = VIRTUAL.lock()
    {
        c.clear();
    }
    if msg == WM_INPUT {
        // SAFETY: RAWINPUT is plain data for which all-zero bytes are valid.
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
        let physical = got != u32::MAX && !raw.header.hDevice.is_null();
        if physical && raw.header.dwType == RIM_TYPEKEYBOARD {
            KB_CLOCK.with(|c| c.raw.set(Some(Instant::now())));
        }
        if physical && raw.header.dwType == RIM_TYPEMOUSE {
            MOUSE_CLOCK.with(|c| c.raw.set(Some(Instant::now())));
            // SAFETY: dwType is RIM_TYPEMOUSE, so the mouse arm of the union is active.
            let mouse = unsafe { raw.data.mouse };
            let (dx, dy) = (mouse.lLastX, mouse.lLastY);
            if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 && (dx, dy) != (0, 0) {
                let (x, y) = cursor_local();
                CTX.with_borrow_mut(|c| {
                    if let Some(ctx) = c {
                        // Raw deltas skip the user's pointer settings; apply them so a peer
                        // moves like the local cursor.
                        let now = Instant::now();
                        let dt_ms = now.duration_since(ctx.last_move).as_secs_f64() * 1000.0;
                        ctx.last_move = now;
                        let (dx, dy) = ctx.ballistics.apply(dx, dy, dt_ms);
                        send(ctx, Captured::Moved { dx, dy, x, y });
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
    /// Shared with the hooks; see [`Capture::set_swallow`].
    swallow: Arc<AtomicBool>,
    /// Win32 id of the capture thread, the target of the `WM_QUIT` sent on drop.
    thread_id: u32,
    /// The capture thread; `None` only after it has been joined.
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Starts capture on its own message-loop thread, delivering every event to `tx` on it.
    /// Input reaches local apps unmodified until [`Capture::set_swallow`] is turned on.
    /// Waits until the hooks and Raw Input are in place, so an error is reported here
    /// rather than lost on the thread.
    pub fn start_with(tx: Sink) -> Result<Self, WinError> {
        if let Some(w) = ensure_dpi_aware() {
            tx.warnings().raise(w);
        }
        let (ready_tx, ready_rx) = channel();
        let swallow = Arc::new(AtomicBool::new(false));
        let swallow_t = swallow.clone();
        let thread = std::thread::Builder::new()
            .name("overdesk-win-capture".into())
            .spawn(move || run_capture(tx, swallow_t, ready_tx))
            .map_err(|_| WinError::HookFailed)?;
        let thread_id = ready_rx.recv().map_err(|_| WinError::HookFailed)??;
        Ok(Self {
            swallow,
            thread_id,
            thread: Some(thread),
        })
    }

    /// While on, input is consumed instead of reaching local apps, and the cursor stays put.
    pub fn set_swallow(&self, on: bool) {
        self.swallow.store(on, Ordering::Relaxed);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            quit_thread(t, self.thread_id);
        }
    }
}

/// Reads one `SmoothMouse*Curve` value: five 8-byte points whose low DWORD is 16.16 fixed
/// point. `None` if absent or not exactly 40 bytes.
fn read_curve(name: &str) -> Option<[u32; 5]> {
    let key = wide("Control Panel\\Mouse");
    let name = wide(name);
    let mut buf = [0u8; 40];
    let mut len = buf.len() as u32;
    // SAFETY: both strings are NUL-terminated; `buf` has `len` writable bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_BINARY,
            null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (rc == 0 && len == 40).then(|| {
        std::array::from_fn(|i| u32::from_le_bytes([buf[i * 8], buf[i * 8 + 1], buf[i * 8 + 2], buf[i * 8 + 3]]))
    })
}

/// The user's pointer speed, "Enhance pointer precision" and curves, plus the system DPI
/// factor. Falls back to speed 10 (no scaling) when a setting cannot be read.
fn read_ballistics() -> Ballistics {
    let mut speed: i32 = 10;
    let mut accel = [0i32; 3]; // threshold1, threshold2, acceleration
    // SAFETY: SPI_GETMOUSESPEED writes one i32 and SPI_GETMOUSE three, matching the buffers.
    let (ok_speed, ok_accel) = unsafe {
        (
            SystemParametersInfoW(SPI_GETMOUSESPEED, 0, (&mut speed as *mut i32).cast(), 0),
            SystemParametersInfoW(SPI_GETMOUSE, 0, accel.as_mut_ptr().cast(), 0),
        )
    };
    if ok_speed == 0 {
        speed = 10;
    }
    // SAFETY: plain Win32 call; the process is DPI aware, so this is the real system DPI.
    let dpi = unsafe { GetDpiForSystem() };
    let dpi_factor = if dpi == 0 { 1.0 } else { 96.0 / f64::from(dpi) };
    let curve = read_curve("SmoothMouseXCurve").zip(read_curve("SmoothMouseYCurve"));
    Ballistics::new(speed, ok_accel != 0 && accel[2] != 0, dpi_factor, curve)
}

/// Unhooks a low-level hook when dropped (a null hook is nothing to remove).
struct HookGuard(HHOOK);

impl Drop for HookGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from `SetWindowsHookExW` on this thread and is removed once.
            unsafe { UnhookWindowsHookEx(self.0) };
        }
    }
}

/// Clears the capture thread's [`CTX`] and [`CAPS`] however the thread ends.
struct CtxGuard;

impl Drop for CtxGuard {
    fn drop(&mut self) {
        CAPS.store(-1, Ordering::Relaxed);
        let _ = CTX.try_with(|c| {
            if let Ok(mut c) = c.try_borrow_mut() {
                *c = None;
            }
        });
    }
}

/// How often the low-level hooks are checked. Windows silently removes a low-level hook whose
/// callback ever ran past its time limit (and gives no notification), after which capture and
/// swallowing would quietly stop. Raw Input keeps arriving, so a hook that has not run while
/// its device reported (see [`hook_is_dead`]) is reinstalled, within about this long.
const CHECK_EVERY_MS: u32 = 1_000;
/// The id of the hook check timer on the Raw Input window.
const CHECK_TIMER: usize = 1;

/// Replaces `hook` with a fresh installation of `procedure` and restarts `clock` from now. The
/// old one is removed first: two live hooks would deliver every event twice. A failure leaves
/// a null hook, which [`recheck`] tries again.
fn reinstall(
    hook: &mut HookGuard,
    id: i32,
    procedure: unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT,
    clock: &'static std::thread::LocalKey<HookClock>,
) {
    clock.with(|c| c.hook.set(Instant::now()));
    drop(std::mem::replace(hook, HookGuard(null_mut())));
    // SAFETY: `procedure` has the hook signature; a null module with thread id 0 is valid
    // for the low-level hooks, which run in the installing thread's message loop.
    let new = unsafe { SetWindowsHookExW(id, Some(procedure), GetModuleHandleW(null()), 0) };
    *hook = HookGuard(new);
}

/// Reinstalls `hook` if it is missing or [`hook_is_dead`]; a live hook is left alone, so no
/// event slips past it.
fn recheck(
    hook: &mut HookGuard,
    id: i32,
    procedure: unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT,
    clock: &'static std::thread::LocalKey<HookClock>,
) {
    if hook.0.is_null() || clock.with(|c| hook_is_dead(c.hook.get(), c.raw.get())) {
        reinstall(hook, id, procedure, clock);
    }
}

/// Body of the capture thread: installs [`CTX`], creates the Raw Input window,
/// registers the mouse and keyboard for Raw Input and installs both low-level hooks, reports its
/// thread id (or the first failure) on `ready`, then pumps messages until `WM_QUIT`.
/// Low-level hooks only run while their installing thread pumps messages. Whatever way it
/// returns, the hooks are removed, the window destroyed and the context cleared.
fn run_capture(tx: Sink, swallow: Arc<AtomicBool>, ready: Sender<Result<u32, WinError>>) {
    let _ctx = CtxGuard;
    CTX.with_borrow_mut(|c| {
        *c = Some(HookCtx {
            tx,
            swallow,
            local: LocalDowns::default(),
            chord: EscapeChord::default(),
            ballistics: read_ballistics(),
            last_move: Instant::now(),
            caps: CapsTracker::new(false), // seeded below, once the thread has a message queue
        })
    });
    let window = message_window("OverdeskRawInput", raw_input_window);
    if window.0.is_null() {
        let _ = ready.send(Err(WinError::WindowFailed));
        return;
    }
    // Only now does this thread have a message queue (creating the window made it); before,
    // `GetKeyState` would answer from a stale snapshot. No hook is installed yet, so no key
    // can change Caps Lock between this read and the tracker starting.
    // SAFETY: a plain query on this thread.
    let caps = unsafe { GetKeyState(0x14) } & 1 != 0;
    CAPS.store(i8::from(caps), Ordering::Relaxed);
    CTX.with_borrow_mut(|c| {
        if let Some(ctx) = c {
            ctx.caps = CapsTracker::new(caps);
        }
    });
    // HID usage page 1 (Generic Desktop), usage 2 (Mouse) and 6 (Keyboard). `RIDEV_INPUTSINK`
    // delivers input even though this window never has focus.
    let device = |usage| RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: usage,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: window.0,
    };
    let devices = [device(0x02), device(0x06)];
    // SAFETY: `devices` holds two valid RAWINPUTDEVICEs and the size matches.
    if unsafe { RegisterRawInputDevices(devices.as_ptr(), 2, std::mem::size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
        let _ = ready.send(Err(WinError::RawInputFailed));
        return;
    }
    let (mut kb, mut mouse) = (HookGuard(null_mut()), HookGuard(null_mut()));
    reinstall(&mut kb, WH_KEYBOARD_LL, keyboard_hook, &KB_CLOCK);
    reinstall(&mut mouse, WH_MOUSE_LL, mouse_hook, &MOUSE_CLOCK);
    if kb.0.is_null() || mouse.0.is_null() {
        let _ = ready.send(Err(WinError::HookFailed));
        return;
    }
    // SAFETY: the window is ours and the timer is killed with it.
    unsafe { SetTimer(window.0, CHECK_TIMER, CHECK_EVERY_MS, None) };
    // SAFETY: plain Win32 call.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    // Low-level hooks are called through this thread's message loop.
    pump(|msg| {
        let check = msg.message == WM_TIMER && msg.wParam == CHECK_TIMER;
        if check {
            recheck(&mut kb, WH_KEYBOARD_LL, keyboard_hook, &KB_CLOCK);
            recheck(&mut mouse, WH_MOUSE_LL, mouse_hook, &MOUSE_CLOCK);
        }
        check
    });
    // `mouse`, `kb`, `window` and `_ctx` drop here, in that order (reverse of declaration).
}
