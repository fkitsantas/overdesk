// SPDX-License-Identifier: AGPL-3.0-only
//! Quartz event tap (capture) and event posting (inject). Kept thin: every decision
//! lives in the pure `events` / `keymap` modules.
#![allow(unsafe_code)] // FFI boundary; each unsafe block states its invariant.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{NonNull, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFString, CFType, CGPoint, kCFBooleanTrue, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapCallBack, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton, CGPreflightListenEventAccess,
    CGPreflightPostEventAccess, CGRequestListenEventAccess, CGRequestPostEventAccess, CGScrollEventUnit,
};
use overdesk_proto::{Button, Phase, ScrollUnit};

use super::events::{
    ClickCounter, Held, LocalDowns, ModChange, MoveKind, Subpixel, flags_for, modifier_change, move_kind,
    phase_from_cg, phase_to_cg,
};
use super::keymap::{code_to_vk, vk_to_code};
use crate::input::EscapeChord;

/// Stamped into `kCGEventSourceUserData` of every event we post, so our own tap ignores
/// it. Every peer both captures and injects; without this, input would loop forever.
const INJECTED_TAG: i64 = 0x4F56_4552_4445_534B; // "OVERDESK"

/// shortcut: fixed 500 ms; read the user's `NSEvent.doubleClickInterval` once AppKit is linked.
const DOUBLE_CLICK_MS: u64 = 500;

#[derive(Debug, PartialEq, Eq)]
pub enum MacError {
    /// Grant in System Settings › Privacy & Security › Input Monitoring.
    NoListenPermission,
    /// Grant in System Settings › Privacy & Security › Accessibility.
    NoPostPermission,
    TapCreateFailed,
    SourceCreateFailed,
}

/// Main display size in points.
/// shortcut: main display only; multi-display Macs need per-display screens on the Desk.
pub fn main_display_size() -> (u32, u32) {
    let r = objc2_core_graphics::CGDisplayBounds(objc2_core_graphics::CGMainDisplayID());
    (r.size.width as u32, r.size.height as u32)
}

/// `(may capture, may inject)` without prompting.
pub fn permissions() -> (bool, bool) {
    (CGPreflightListenEventAccess(), CGPreflightPostEventAccess())
}

/// Shows the system permission prompts for anything not yet granted.
pub fn request_permissions() {
    let (listen, post) = permissions();
    if !listen {
        CGRequestListenEventAccess();
    }
    if !post {
        CGRequestPostEventAccess();
    }
}

pub use crate::input::Captured;

struct TapCtx {
    tx: Sender<Captured>,
    swallow: Arc<AtomicBool>,
    port: OnceCell<CFRetained<CFMachPort>>,
    local: RefCell<LocalDowns>,
    sub: RefCell<Subpixel>,
    chord: RefCell<EscapeChord>,
    /// Whether this tap last hid the cursor.
    detached: Cell<bool>,
}

unsafe extern "C" {
    // Private SkyLight/CoreGraphics API. A background process cannot hide the cursor
    // (or stop it moving) through any public API; Deskflow, Barrier and Synergy rely on
    // this property for the same reason. If a macOS release breaks it, only the cursor's
    // visibility while on another device is affected.
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(cid: i32, target: i32, key: &CFString, value: &CFType) -> i32;
}

/// Lets this background process hide the cursor. Called once per tap.
fn allow_background_cursor_hiding() {
    // SAFETY: plain C calls with a valid connection id and live CF objects.
    unsafe {
        let cid = _CGSDefaultConnection();
        if let Some(t) = kCFBooleanTrue {
            CGSSetConnectionProperty(cid, cid, &CFString::from_static_str("SetsCursorInBackground"), t);
        }
    }
}

/// While swallowing, the Mac's cursor is hidden: swallowed moves still move it, and no
/// public API stops that for a background process. Hide and show calls are counted by
/// macOS, so callers keep them balanced via `TapCtx::detached`.
fn detach_cursor(on: bool) {
    let display = objc2_core_graphics::CGMainDisplayID();
    let _ = if on {
        objc2_core_graphics::CGDisplayHideCursor(display)
    } else {
        objc2_core_graphics::CGDisplayShowCursor(display)
    };
}

/// `kCGEventFlagMaskAlphaShift`: Caps Lock state, owned by the system, never by us.
const FLAG_CAPS_LOCK: u64 = 0x0001_0000;

/// A running event tap. Dropping it stops the tap thread.
pub struct Capture {
    stop: Arc<AtomicBool>,
    swallow: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Starts the tap on its own run-loop thread. Events are delivered unmodified to
    /// local apps until [`Capture::set_swallow`] is turned on.
    pub fn start() -> Result<(Self, Receiver<Captured>), MacError> {
        // An active (event-consuming) tap needs Accessibility as well as Input Monitoring.
        match permissions() {
            (false, _) => return Err(MacError::NoListenPermission),
            (_, false) => return Err(MacError::NoPostPermission),
            _ => {}
        }
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let swallow = Arc::new(AtomicBool::new(false));
        let (stop_t, swallow_t) = (stop.clone(), swallow.clone());
        let thread = std::thread::Builder::new()
            .name("overdesk-mac-tap".into())
            .spawn(move || run_tap(tx, swallow_t, stop_t, ready_tx))
            .map_err(|_| MacError::TapCreateFailed)?;
        ready_rx.recv().map_err(|_| MacError::TapCreateFailed)??;
        Ok((
            Self {
                stop,
                swallow,
                thread: Some(thread),
            },
            rx,
        ))
    }

    /// While on, captured input is consumed instead of reaching local apps
    /// (focus is on another peer).
    pub fn set_swallow(&self, on: bool) {
        // The tap hides or shows the cursor on its next event, keeping the counts balanced.
        self.swallow.store(on, Ordering::Relaxed);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run_tap(tx: Sender<Captured>, swallow: Arc<AtomicBool>, stop: Arc<AtomicBool>, ready: Sender<Result<(), MacError>>) {
    let mask = [
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
        CGEventType::ScrollWheel,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::OtherMouseDragged,
    ]
    .iter()
    .fold(0u64, |m, t| m | (1 << t.0));

    // Lives until this thread ends; only this thread's run loop touches it.
    let ctx = Box::new(TapCtx {
        tx,
        swallow,
        port: OnceCell::new(),
        local: RefCell::new(LocalDowns::default()),
        sub: RefCell::new(Subpixel::default()),
        chord: RefCell::new(EscapeChord::default()),
        detached: Cell::new(false),
    });
    let ctx_ptr = &*ctx as *const TapCtx as *mut c_void;
    let callback: CGEventTapCallBack = Some(tap_callback);
    // SAFETY: `tap_callback` matches the C signature, and `ctx_ptr` outlives the tap
    // because the tap and run-loop source are dropped before `ctx` at function exit.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::HIDEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            callback,
            ctx_ptr,
        )
    };
    let Some(port) = port else {
        let _ = ready.send(Err(MacError::TapCreateFailed));
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        let _ = ready.send(Err(MacError::TapCreateFailed));
        return;
    };
    let _ = ctx.port.set(port.clone());
    let Some(run_loop) = CFRunLoop::current() else {
        let _ = ready.send(Err(MacError::TapCreateFailed));
        return;
    };
    // SAFETY: reading an immutable CoreFoundation constant.
    let mode = unsafe { kCFRunLoopDefaultMode };
    run_loop.add_source(Some(&source), mode);
    CGEvent::tap_enable(&port, true);
    allow_background_cursor_hiding();
    let _ = ready.send(Ok(()));

    // Poll the stop flag between short run-loop slices instead of sharing the
    // run loop across threads.
    while !stop.load(Ordering::Relaxed) {
        CFRunLoop::run_in_mode(mode, 0.1, false);
    }
    CGEvent::tap_enable(&port, false);
    if ctx.detached.get() {
        detach_cursor(false);
    }
    run_loop.remove_source(Some(&source), mode);
    port.invalidate();
    drop(source);
    drop(port);
    drop(ctx);
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    let raw = event.as_ptr();
    // A panic must never unwind into WindowServer code; on panic, let the event through.
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `info` is the `TapCtx` pointer given to `tap_create`, alive for the tap's life.
        let ctx = unsafe { &*(info as *const TapCtx) };
        // SAFETY: Quartz hands us a valid event for the duration of the callback.
        let ev = unsafe { event.as_ref() };
        handle(ctx, ty, ev, raw)
    }))
    .unwrap_or(raw)
}

fn handle(ctx: &TapCtx, ty: CGEventType, ev: &CGEvent, raw: *mut CGEvent) -> *mut CGEvent {
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        // macOS disables slow taps; turn ourselves back on immediately.
        if let Some(port) = ctx.port.get() {
            CGEvent::tap_enable(port, true);
        }
        return raw;
    }
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == INJECTED_TAG {
        return raw;
    }
    let edge = translate(ty, ev, ctx);
    if let Some((Held::Key(code), down)) = edge
        && ctx.chord.borrow_mut().key(code, down)
    {
        // Failsafe: give input back to this Mac before anything else can go wrong.
        ctx.swallow.store(false, Ordering::Relaxed);
        let _ = ctx.tx.send(Captured::Escape);
    }
    let swallowing = ctx.swallow.load(Ordering::Relaxed);
    // Reconciled here so every path that ends swallowing (engine, escape chord, dead
    // receiver) also shows the cursor again.
    if ctx.detached.get() != swallowing {
        detach_cursor(swallowing);
        ctx.detached.set(swallowing);
    }
    let deliver = match edge {
        Some((held, down)) => ctx.local.borrow_mut().deliver_locally(held, down, swallowing),
        None => !swallowing,
    };
    if deliver { raw } else { null_mut() }
}

fn field(ev: &CGEvent, f: CGEventField) -> i64 {
    CGEvent::integer_value_field(Some(ev), f)
}

/// Quartz button number: 0 left, 1 right, 2 middle, 3 back, 4 forward.
fn button_number(b: Button) -> i64 {
    match b {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        Button::Back => 3,
        Button::Forward => 4,
    }
}

fn other_button(n: i64) -> Option<Button> {
    match n {
        2 => Some(Button::Middle),
        3 => Some(Button::Back),
        4 => Some(Button::Forward),
        _ => None,
    }
}

/// Sends captured input to the channel and returns the press/release edge it
/// represents, if any. Sends never block (unbounded channel), keeping the tap well
/// under its timeout.
fn translate(ty: CGEventType, ev: &CGEvent, ctx: &TapCtx) -> Option<(Held, bool)> {
    let send = |c| {
        // Nobody is listening any more (engine gone): never keep swallowing input.
        if ctx.tx.send(c).is_err() {
            ctx.swallow.store(false, Ordering::Relaxed);
        }
    };
    let button = |button, pressed| {
        send(Captured::Button { button, pressed });
        Some((Held::Button(button), pressed))
    };
    let key = |code, pressed| {
        send(Captured::Key { code, pressed });
        Some((Held::Key(code), pressed))
    };
    match ty {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            // The double fields keep sub-pixel motion that the integer fields round away.
            let (dx, dy) = ctx.sub.borrow_mut().feed(
                CGEvent::double_value_field(Some(ev), CGEventField::MouseEventDeltaX),
                CGEvent::double_value_field(Some(ev), CGEventField::MouseEventDeltaY),
            );
            if (dx, dy) != (0, 0) {
                let at = CGEvent::location(Some(ev));
                send(Captured::Moved {
                    dx,
                    dy,
                    x: at.x as i32,
                    y: at.y as i32,
                });
            }
            None
        }
        CGEventType::LeftMouseDown => button(Button::Left, true),
        CGEventType::LeftMouseUp => button(Button::Left, false),
        CGEventType::RightMouseDown => button(Button::Right, true),
        CGEventType::RightMouseUp => button(Button::Right, false),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            other_button(field(ev, CGEventField::MouseEventButtonNumber))
                .and_then(|b| button(b, ty == CGEventType::OtherMouseDown))
        }
        // Repeats are forwarded: an injected key-down does not auto-repeat on the
        // receiving OS, so the source keyboard's own repeat rate is what the user expects.
        CGEventType::KeyDown | CGEventType::KeyUp => vk_to_code(field(ev, CGEventField::KeyboardEventKeycode) as u16)
            .and_then(|code| key(code, ty == CGEventType::KeyDown)),
        CGEventType::FlagsChanged => {
            let vk = field(ev, CGEventField::KeyboardEventKeycode) as u16;
            match modifier_change(vk, CGEvent::flags(Some(ev)).0) {
                Some(ModChange::Press(code)) => key(code, true),
                Some(ModChange::Release(code)) => key(code, false),
                Some(ModChange::Toggle(code)) => {
                    send(Captured::Key { code, pressed: true });
                    send(Captured::Key { code, pressed: false });
                    None
                }
                None => None,
            }
        }
        CGEventType::ScrollWheel => {
            let phase = phase_from_cg(
                field(ev, CGEventField::ScrollWheelEventScrollPhase),
                field(ev, CGEventField::ScrollWheelEventMomentumPhase),
            );
            // Quartz axis 2 is positive leftwards; the wire uses positive = right.
            let (dx, dy, unit) = if field(ev, CGEventField::ScrollWheelEventIsContinuous) != 0 {
                (
                    -field(ev, CGEventField::ScrollWheelEventPointDeltaAxis2),
                    field(ev, CGEventField::ScrollWheelEventPointDeltaAxis1),
                    ScrollUnit::Pixel,
                )
            } else {
                (
                    -field(ev, CGEventField::ScrollWheelEventDeltaAxis2) * 120,
                    field(ev, CGEventField::ScrollWheelEventDeltaAxis1) * 120,
                    ScrollUnit::Notch120,
                )
            };
            send(Captured::Scroll {
                dx: dx as i32,
                dy: dy as i32,
                unit,
                phase,
            });
            None
        }
        _ => None,
    }
}

/// Posts input that apps cannot tell apart from real Apple hardware.
pub struct Injector {
    source: CFRetained<CGEventSource>,
    keys: Vec<u32>,
    buttons: Vec<Button>,
    clicks: ClickCounter,
    last_click_count: i64,
    pos: CGPoint,
    epoch: Instant,
}

impl Injector {
    pub fn new() -> Result<Self, MacError> {
        if !permissions().1 {
            return Err(MacError::NoPostPermission);
        }
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok_or(MacError::SourceCreateFailed)?;
        let pos = CGEvent::new(None)
            .map(|e| CGEvent::location(Some(&e)))
            .unwrap_or(CGPoint { x: 0.0, y: 0.0 });
        Ok(Self {
            source,
            keys: Vec::new(),
            buttons: Vec::new(),
            clicks: ClickCounter::default(),
            last_click_count: 1,
            pos,
            epoch: Instant::now(),
        })
    }

    fn post(&self, ev: &CGEvent) {
        CGEvent::set_integer_value_field(Some(ev), CGEventField::EventSourceUserData, INJECTED_TAG);
        let caps = CGEvent::flags(Some(ev)).0 & FLAG_CAPS_LOCK;
        CGEvent::set_flags(Some(ev), CGEventFlags(flags_for(&self.keys) | caps));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(ev));
    }

    /// Moves the pointer to global display coordinates (points, top-left origin).
    pub fn pointer(&mut self, x: f64, y: f64) {
        let (dx, dy) = (x - self.pos.x, y - self.pos.y);
        self.pos = CGPoint { x, y };
        let (ty, button) = match move_kind(&self.buttons) {
            MoveKind::Moved => (CGEventType::MouseMoved, CGMouseButton::Left),
            MoveKind::LeftDragged => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            MoveKind::RightDragged => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            MoveKind::OtherDragged => {
                let held = self.buttons.iter().find(|b| !matches!(b, Button::Left | Button::Right));
                (
                    CGEventType::OtherMouseDragged,
                    CGMouseButton(held.map_or(2, |b| button_number(*b)) as u32),
                )
            }
        };
        if let Some(ev) = CGEvent::new_mouse_event(Some(&self.source), ty, self.pos, button) {
            // Games, 3D tools and some AppKit drags read relative deltas, not position.
            CGEvent::set_double_value_field(Some(&ev), CGEventField::MouseEventDeltaX, dx);
            CGEvent::set_double_value_field(Some(&ev), CGEventField::MouseEventDeltaY, dy);
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventButtonNumber, button.0 as i64);
            self.post(&ev);
        }
    }

    /// Where the system cursor actually is now (global points).
    pub fn cursor() -> (f64, f64) {
        let p = CGEvent::new(None)
            .map(|e| CGEvent::location(Some(&e)))
            .unwrap_or(CGPoint { x: 0.0, y: 0.0 });
        (p.x, p.y)
    }

    /// Releases every key and button this injector holds down. Call when focus leaves
    /// this Mac or the peer driving it disconnects.
    pub fn release_all(&mut self) {
        for code in self.keys.clone().into_iter().rev() {
            self.key(code, false);
        }
        for button in self.buttons.clone() {
            self.button(button, false);
        }
    }

    pub fn button(&mut self, button: Button, pressed: bool) {
        let (ty, cg_button, number) = match (button, pressed) {
            (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
            (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
            (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
            (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
            (b, p) => {
                let n = button_number(b);
                let ty = if p {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                };
                (ty, CGMouseButton(n as u32), n)
            }
        };
        if pressed {
            let t_ms = self.epoch.elapsed().as_millis() as u64;
            self.last_click_count =
                self.clicks
                    .press(button, t_ms, self.pos.x as i32, self.pos.y as i32, DOUBLE_CLICK_MS);
            if !self.buttons.contains(&button) {
                self.buttons.push(button);
            }
        } else {
            self.buttons.retain(|b| *b != button);
        }
        if let Some(ev) = CGEvent::new_mouse_event(Some(&self.source), ty, self.pos, cg_button) {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventClickState, self.last_click_count);
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventButtonNumber, number);
            self.post(&ev);
        }
    }

    /// Returns `false` for codes with no macOS key (media keys are not posted yet).
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        let Some(vk) = code_to_vk(code) else { return false };
        // A press of a key already down is a repeat, and apps expect it marked as one.
        let repeat = pressed && self.keys.contains(&code);
        // Update held state first: a modifier's own event carries its new flag state.
        if pressed {
            if !repeat {
                self.keys.push(code);
            }
        } else {
            self.keys.retain(|k| *k != code);
        }
        match CGEvent::new_keyboard_event(Some(&self.source), vk, pressed) {
            Some(ev) => {
                CGEvent::set_integer_value_field(Some(&ev), CGEventField::KeyboardEventAutorepeat, repeat as i64);
                self.post(&ev);
                true
            }
            None => false,
        }
    }

    pub fn scroll(&mut self, dx: i32, dy: i32, unit: ScrollUnit, phase: Phase) {
        let (cg_unit, dx, dy) = match unit {
            ScrollUnit::Pixel => (CGScrollEventUnit::Pixel, dx, dy),
            // shortcut: drops sub-notch remainders from high-resolution wheels; accumulate them if users notice.
            ScrollUnit::Notch120 => (CGScrollEventUnit::Line, dx / 120, dy / 120),
        };
        let Some(ev) = CGEvent::new_scroll_wheel_event2(Some(&self.source), cg_unit, 2, dy, -dx, 0) else {
            return;
        };
        let (scroll_phase, momentum_phase) = phase_to_cg(phase);
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::ScrollWheelEventScrollPhase, scroll_phase);
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::ScrollWheelEventMomentumPhase, momentum_phase);
        if unit == ScrollUnit::Pixel {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::ScrollWheelEventIsContinuous, 1);
        }
        self.post(&ev);
    }
}
