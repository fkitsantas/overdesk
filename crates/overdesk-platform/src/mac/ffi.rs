// SPDX-License-Identifier: AGPL-3.0-only
//! Quartz event tap (capture) and event posting (inject). Kept thin: every decision
//! lives in the pure `events` / `keymap` modules.
#![allow(unsafe_code)] // FFI boundary; each unsafe block states its invariant.

use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CGPoint, kCFRunLoopDefaultMode};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapCallBack, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton, CGPreflightListenEventAccess,
    CGPreflightPostEventAccess, CGRequestListenEventAccess, CGRequestPostEventAccess, CGScrollEventUnit,
};
use overdesk_proto::{Button, Phase, ScrollUnit};

use super::events::{
    ClickCounter, ModChange, MoveKind, flags_for, modifier_change, move_kind, phase_from_cg, phase_to_cg,
};
use super::keymap::{code_to_vk, vk_to_code};

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

/// Physical input seen on this Mac, in Overdesk terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Captured {
    /// Relative motion, for `Switcher::move_by`.
    Moved {
        dx: i32,
        dy: i32,
    },
    Button {
        button: Button,
        pressed: bool,
    },
    Key {
        code: u32,
        pressed: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
        unit: ScrollUnit,
        phase: Phase,
    },
}

struct TapCtx {
    tx: Sender<Captured>,
    swallow: Arc<AtomicBool>,
    port: std::cell::OnceCell<CFRetained<CFMachPort>>,
}

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
        if !permissions().0 {
            return Err(MacError::NoListenPermission);
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
        port: std::cell::OnceCell::new(),
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
    let _ = ready.send(Ok(()));

    // Poll the stop flag between short run-loop slices instead of sharing the
    // run loop across threads.
    while !stop.load(Ordering::Relaxed) {
        CFRunLoop::run_in_mode(mode, 0.1, false);
    }
    CGEvent::tap_enable(&port, false);
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
    // SAFETY: `info` is the `TapCtx` pointer given to `tap_create`, alive for the tap's life.
    let ctx = unsafe { &*(info as *const TapCtx) };
    let raw = event.as_ptr();
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        // macOS disables slow taps; turn ourselves back on immediately.
        if let Some(port) = ctx.port.get() {
            CGEvent::tap_enable(port, true);
        }
        return raw;
    }
    // SAFETY: Quartz hands us a valid event for the duration of the callback.
    let ev = unsafe { event.as_ref() };
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == INJECTED_TAG {
        return raw;
    }
    translate(ty, ev, &ctx.tx);
    if ctx.swallow.load(Ordering::Relaxed) {
        null_mut()
    } else {
        raw
    }
}

fn field(ev: &CGEvent, f: CGEventField) -> i64 {
    CGEvent::integer_value_field(Some(ev), f)
}

fn other_button(n: i64) -> Option<Button> {
    match n {
        2 => Some(Button::Middle),
        3 => Some(Button::Back),
        4 => Some(Button::Forward),
        _ => None,
    }
}

/// Sends never block (unbounded channel), keeping the tap well under its timeout.
fn translate(ty: CGEventType, ev: &CGEvent, tx: &Sender<Captured>) {
    let send = |c| {
        let _ = tx.send(c);
    };
    let button = |button, pressed| send(Captured::Button { button, pressed });
    match ty {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => send(Captured::Moved {
            dx: field(ev, CGEventField::MouseEventDeltaX) as i32,
            dy: field(ev, CGEventField::MouseEventDeltaY) as i32,
        }),
        CGEventType::LeftMouseDown => button(Button::Left, true),
        CGEventType::LeftMouseUp => button(Button::Left, false),
        CGEventType::RightMouseDown => button(Button::Right, true),
        CGEventType::RightMouseUp => button(Button::Right, false),
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            if let Some(b) = other_button(field(ev, CGEventField::MouseEventButtonNumber)) {
                button(b, ty == CGEventType::OtherMouseDown);
            }
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            // The receiving OS generates its own key repeat; forwarding ours would double it.
            if field(ev, CGEventField::KeyboardEventAutorepeat) != 0 {
                return;
            }
            if let Some(code) = vk_to_code(field(ev, CGEventField::KeyboardEventKeycode) as u16) {
                send(Captured::Key {
                    code,
                    pressed: ty == CGEventType::KeyDown,
                });
            }
        }
        CGEventType::FlagsChanged => {
            let vk = field(ev, CGEventField::KeyboardEventKeycode) as u16;
            match modifier_change(vk, CGEvent::flags(Some(ev)).0) {
                Some(ModChange::Press(code)) => send(Captured::Key { code, pressed: true }),
                Some(ModChange::Release(code)) => send(Captured::Key { code, pressed: false }),
                Some(ModChange::Toggle(code)) => {
                    send(Captured::Key { code, pressed: true });
                    send(Captured::Key { code, pressed: false });
                }
                None => {}
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
        }
        _ => {}
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
        CGEvent::set_flags(Some(ev), CGEventFlags(flags_for(&self.keys)));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(ev));
    }

    /// Moves the pointer to global display coordinates (points, top-left origin).
    pub fn pointer(&mut self, x: f64, y: f64) {
        self.pos = CGPoint { x, y };
        let (ty, button) = match move_kind(&self.buttons) {
            MoveKind::Moved => (CGEventType::MouseMoved, CGMouseButton::Left),
            MoveKind::LeftDragged => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            MoveKind::RightDragged => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            MoveKind::OtherDragged => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
        };
        if let Some(ev) = CGEvent::new_mouse_event(Some(&self.source), ty, self.pos, button) {
            self.post(&ev);
        }
    }

    pub fn button(&mut self, button: Button, pressed: bool) {
        let (ty, cg_button, number) = match (button, pressed) {
            (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
            (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
            (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
            (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
            (b, p) => {
                let n = match b {
                    Button::Back => 3,
                    Button::Forward => 4,
                    _ => 2,
                };
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
        // Update held state first: a modifier's own event carries its new flag state.
        if pressed {
            if !self.keys.contains(&code) {
                self.keys.push(code);
            }
        } else {
            self.keys.retain(|k| *k != code);
        }
        match CGEvent::new_keyboard_event(Some(&self.source), vk, pressed) {
            Some(ev) => {
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
