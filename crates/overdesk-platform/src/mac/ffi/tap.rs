// SPDX-License-Identifier: AGPL-3.0-only
//! The Quartz event tap: capture on its own run-loop thread, swallowing, and the escape chord.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{NonNull, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_app_kit::{NSEvent, NSEventPhase, NSEventType};
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFString, CFType, kCFBooleanTrue, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapCallBack, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};
use overdesk_proto::{Button, Gesture, Phase, ScrollUnit};

use super::display::{DisplayWatch, desktop_bounds};
use super::{Captured, INJECTED_TAG, MacError, permissions};
use crate::input::{EscapeChord, Held, LocalDowns, Rect, Sink, Subpixel, UNITS_PER_NOTCH, to_local};
use crate::mac::events::{DockSample, DockSwipe, HID_DOCK_SWIPE, ModChange, modifier_change, phase_from_cg};
use crate::mac::keymap::vk_to_code;

/// State for the event tap callback, passed to Quartz as the tap's `userInfo` pointer.
/// Owned by the tap thread and only touched from its run loop, hence `Cell`/`RefCell`
/// rather than locks.
struct TapCtx {
    /// Where captured input goes; the receiving end belongs to the engine.
    tx: Sink,
    /// Set while focus is on another peer, so local input is consumed. Shared with
    /// [`Capture::set_swallow`].
    swallow: Arc<AtomicBool>,
    /// The tap's Mach port, so the callback can re-enable a tap macOS disabled. Set
    /// once, right after the tap is created.
    port: OnceCell<CFRetained<CFMachPort>>,
    /// Keeps local apps' press/release pairs balanced across swallow changes.
    local: RefCell<LocalDowns>,
    /// Carries fractional pointer motion between events.
    sub: RefCell<Subpixel>,
    /// Failsafe detector for the escape chord.
    chord: RefCell<EscapeChord>,
    /// Whether this tap last hid the cursor.
    detached: Cell<bool>,
    /// Folds a Dock swipe's events into one gesture.
    dock: RefCell<DockSwipe>,
    /// Logs the raw fields of gesture events (`OVERDESK_GESTURE_PROBE`), to confirm them on hardware.
    probe: bool,
}

unsafe extern "C" {
    /// The C library's symbol lookup; `RTLD_DEFAULT` searches everything already loaded.
    fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
}

/// `RTLD_DEFAULT` on macOS: search every image already loaded in the process.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// `_CGSDefaultConnection`: this process's connection id to the WindowServer.
type CgsDefaultConnection = unsafe extern "C" fn() -> i32;
/// `CGSSetConnectionProperty`: sets a property on WindowServer connection `target` on
/// behalf of `cid`; returns a `CGError` (0 on success).
type CgsSetConnectionProperty = unsafe extern "C" fn(cid: i32, target: i32, key: &CFString, value: &CFType) -> i32;

/// Looks up a private SkyLight/CoreGraphics symbol by name; `None` if this macOS lacks it.
fn private_symbol(name: &std::ffi::CStr) -> Option<NonNull<c_void>> {
    // SAFETY: `name` is a valid NUL-terminated string and `RTLD_DEFAULT` a valid handle.
    NonNull::new(unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) })
}

/// Lets this background process hide the cursor. Called once per tap.
///
/// PRIVATE API: a background process cannot hide the cursor (or stop it moving) through any
/// public API; Deskflow, Barrier and Synergy rely on this property for the same reason. The
/// two functions are looked up at run time, not linked, so a macOS release that removes them
/// only leaves the cursor visible while on another device; it never stops the app starting.
fn allow_background_cursor_hiding() {
    let (Some(connection), Some(set)) = (
        private_symbol(c"_CGSDefaultConnection"),
        private_symbol(c"CGSSetConnectionProperty"),
    ) else {
        return;
    };
    // SAFETY: both symbols were just resolved by name from the system frameworks and have
    // exactly these C signatures (they are what Deskflow and Barrier call).
    let (connection, set) = unsafe {
        (
            std::mem::transmute::<*mut c_void, CgsDefaultConnection>(connection.as_ptr()),
            std::mem::transmute::<*mut c_void, CgsSetConnectionProperty>(set.as_ptr()),
        )
    };
    // SAFETY: plain C calls with a valid connection id and live CF objects.
    unsafe {
        let cid = connection();
        if let Some(t) = kCFBooleanTrue {
            set(cid, cid, &CFString::from_static_str("SetsCursorInBackground"), t);
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

/// The tap thread's run loop, handed to [`Capture`] so dropping it can stop the loop.
struct RunLoop(CFRetained<CFRunLoop>);

// SAFETY: `CFRunLoopStop` may be called from any thread (CFRunLoop is thread-safe for it), and it is the only
// thing done with this handle off the tap thread.
unsafe impl Send for RunLoop {}
// SAFETY: as above; `stop` takes `&self` and CoreFoundation synchronises it.
unsafe impl Sync for RunLoop {}

/// A running event tap. Dropping it stops the tap thread.
pub struct Capture {
    /// Asks the tap thread to leave its run loop; set on drop.
    stop: Arc<AtomicBool>,
    /// The tap thread's run loop, stopped on drop.
    run_loop: RunLoop,
    /// Shared with the tap callback; see [`Capture::set_swallow`].
    swallow: Arc<AtomicBool>,
    /// The tap thread; `None` only after it has been joined.
    thread: Option<JoinHandle<()>>,
    /// Keeps the desktop bounds live for the tap's lifetime.
    _watch: DisplayWatch,
}

impl Capture {
    /// Starts the tap on its own run-loop thread, delivering every event to `tx` on it.
    /// Events reach local apps unmodified until [`Capture::set_swallow`] is turned on.
    pub fn start_with(tx: Sink) -> Result<Self, MacError> {
        // An active (event-consuming) tap needs Accessibility as well as Input Monitoring.
        match permissions() {
            (false, _) => return Err(MacError::NoListenPermission),
            (_, false) => return Err(MacError::NoPostPermission),
            _ => {}
        }
        let watch = DisplayWatch::new();
        let (ready_tx, ready_rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let swallow = Arc::new(AtomicBool::new(false));
        let (stop_t, swallow_t) = (stop.clone(), swallow.clone());
        let thread = std::thread::Builder::new()
            .name("overdesk-mac-tap".into())
            .spawn(move || run_tap(tx, swallow_t, stop_t, ready_tx))
            .map_err(|_| MacError::TapCreateFailed)?;
        // Wait for the thread to report: the outer `?` covers a thread that died before
        // answering, the inner one a tap it could not create.
        let run_loop = ready_rx.recv().map_err(|_| MacError::TapCreateFailed)??;
        Ok(Self {
            stop,
            run_loop,
            swallow,
            thread: Some(thread),
            _watch: watch,
        })
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
            // Stopping a run loop that has not started yet does nothing, so repeat it until
            // the thread has left; once it runs, one call is enough and it never polls.
            while !t.is_finished() {
                self.run_loop.0.stop();
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = t.join();
        }
    }
}

/// Body of the tap thread: creates an active `kCGHIDEventTap` tap at the head of the
/// queue for every mouse, key, modifier and scroll event, attaches it to this thread's
/// run loop, reports the run loop (or the failure) on `ready`, then runs the loop until `stop`.
fn run_tap(tx: Sink, swallow: Arc<AtomicBool>, stop: Arc<AtomicBool>, ready: Sender<Result<RunLoop, MacError>>) {
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
        // NSEventType rotate (18), magnify (30), smartMagnify (32): public AppKit
        // numbers, which Quartz uses as event types; the tap mask is just a bit per type.
        // Trackpad gestures may also arrive as the private gesture type 29 (kCGSEventGesture),
        // which AppKit decodes to the same magnify/rotate events.
        // shortcut: if a real pinch arrives as both 29 and 30, it is counted twice; drop one
        // type once a hand test shows which the tap receives.
        CGEventType(18),
        CGEventType(29),
        CGEventType(30),
        // 31 (kCGSEventFluidTouchGesture, NSEventTypeSwipe): navigation swipes, seen by
        // the probe and consumed with the rest while input is swallowed.
        CGEventType(31),
        CGEventType(32),
    ]
    .iter()
    // CGEventMask: bit n set means "deliver events of type n" (CGEventMaskBit).
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
        dock: RefCell::new(DockSwipe::default()),
        probe: std::env::var_os("OVERDESK_GESTURE_PROBE").is_some(),
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
        port.invalidate();
        let _ = ready.send(Err(MacError::TapCreateFailed));
        return;
    };
    let _ = ctx.port.set(port.clone());
    let Some(run_loop) = CFRunLoop::current() else {
        port.invalidate();
        let _ = ready.send(Err(MacError::TapCreateFailed));
        return;
    };
    // SAFETY: reading an immutable CoreFoundation constant.
    let mode = unsafe { kCFRunLoopDefaultMode };
    run_loop.add_source(Some(&source), mode);
    CGEvent::tap_enable(&port, true);
    allow_background_cursor_hiding();
    let _ = ready.send(Ok(RunLoop(run_loop.clone())));

    // Sleeps in the run loop until `Capture::drop` stops it.
    while !stop.load(Ordering::Relaxed) {
        CFRunLoop::run();
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

/// The `CGEventTapCallBack` given to Quartz. Returns the event to let it through, or
/// null to consume it.
///
/// # Safety
///
/// Called only by Quartz on the tap thread, with `info` being the `TapCtx` pointer
/// passed to `CGEvent::tap_create` and `event` valid for the duration of the call.
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

/// Forgets what the tap could not have seen while it was off: key edges missed in that gap
/// would leave the escape chord and the local press/release pairing wrong (a stuck "held"
/// key). If input was being swallowed, the peer that has it cannot be trusted to hand it
/// back, so give it back now and tell the engine.
fn recover_after_disable(ctx: &TapCtx) {
    *ctx.chord.borrow_mut() = EscapeChord::default();
    *ctx.local.borrow_mut() = LocalDowns::default();
    ctx.dock.borrow_mut().reset();
    if ctx.swallow.swap(false, Ordering::Relaxed) {
        let _ = ctx.tx.send(Captured::Escape);
    }
}

/// Whether an event with no Overdesk key or button (an unmapped key, a fifth mouse button)
/// reaches local apps. Releases always pass: its press may have come before swallowing began,
/// and eating the release would leave it stuck down.
fn passes_unmapped(ty: CGEventType, swallowing: bool) -> bool {
    !swallowing || matches!(ty, CGEventType::KeyUp | CGEventType::OtherMouseUp)
}

/// Safe body of [`tap_callback`]: re-enables a disabled tap, skips our own injected
/// events, forwards the rest, checks the escape chord and decides whether `raw` reaches
/// local apps (returned as is) or is consumed (null).
fn handle(ctx: &TapCtx, ty: CGEventType, ev: &CGEvent, raw: *mut CGEvent) -> *mut CGEvent {
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        // macOS disables slow taps; turn ourselves back on immediately.
        if let Some(port) = ctx.port.get() {
            CGEvent::tap_enable(port, true);
        }
        recover_after_disable(ctx);
        return raw;
    }
    if CGEvent::integer_value_field(Some(ev), CGEventField::EventSourceUserData) == INJECTED_TAG {
        return raw;
    }
    if ctx.probe && matches!(ty.0, 18 | 29..=32) {
        probe_log(ty, ev);
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
        None => passes_unmapped(ty, swallowing),
    };
    if !deliver && swallowing && is_dock_swipe(ty, ev) && field(ev, DOCK_PHASE) == 4 {
        // shortcut: on macOS 27 the Dock needs a swipe's end to close its gesture state (as the
        // open ISS project found), so the end passes with its motion zeroed; unverified, hand test.
        for f in [DOCK_PROGRESS, DOCK_VELOCITY_X, DOCK_VELOCITY_Y] {
            CGEvent::set_double_value_field(Some(ev), f, 0.0);
        }
        return raw;
    }
    if deliver { raw } else { null_mut() }
}

/// `kCGEventGestureHIDType`: what kind of gesture a type 29/30 event carries.
const DOCK_HID_TYPE: CGEventField = CGEventField(110);
/// `kCGEventGestureSwipeMotion`: 1 sideways, 2 vertical, 3 pinch.
const DOCK_MOTION: CGEventField = CGEventField(123);
/// `kCGEventGestureSwipeProgress`: signed swipe progress.
const DOCK_PROGRESS: CGEventField = CGEventField(124);
/// `kCGEventGestureSwipeVelocityX`.
const DOCK_VELOCITY_X: CGEventField = CGEventField(129);
/// `kCGEventGestureSwipeVelocityY`.
const DOCK_VELOCITY_Y: CGEventField = CGEventField(130);
/// `kCGEventGesturePhase`.
const DOCK_PHASE: CGEventField = CGEventField(132);
/// `kCGEventGestureSwipeValue`: the direction mask of a navigation swipe.
const SWIPE_VALUE: CGEventField = CGEventField(115);
/// The event's own CGS type (`kCGSEventTypeField`), which can differ from the tap's `type`.
const CGS_TYPE: CGEventField = CGEventField(55);

/// Whether `ev` is a Dock swipe (CGS type 30 is also the public magnify type, told apart by its HID type).
fn is_dock_swipe(ty: CGEventType, ev: &CGEvent) -> bool {
    ty.0 == 30 && field(ev, DOCK_HID_TYPE) == HID_DOCK_SWIPE
}

/// Logs what a gesture event carries, never anything the user typed (see `OVERDESK_GESTURE_PROBE`).
fn probe_log(ty: CGEventType, ev: &CGEvent) {
    let f = |x| CGEvent::double_value_field(Some(ev), x);
    overdesk_proto::log::info(format_args!(
        "gesture probe: type={} cgs={} hid={} motion={} phase={} progress={:.4} vx={:.2} vy={:.2} swipe={}",
        ty.0,
        field(ev, CGS_TYPE),
        field(ev, DOCK_HID_TYPE),
        field(ev, DOCK_MOTION),
        field(ev, DOCK_PHASE),
        f(DOCK_PROGRESS),
        f(DOCK_VELOCITY_X),
        f(DOCK_VELOCITY_Y),
        field(ev, SWIPE_VALUE),
    ));
}

/// Reads integer field `f` of `ev` (`CGEventGetIntegerValueField`).
fn field(ev: &CGEvent, f: CGEventField) -> i64 {
    CGEvent::integer_value_field(Some(ev), f)
}

/// Maps the `kCGMouseEventButtonNumber` of an "other" mouse event to a button.
/// Buttons above 4 have no Overdesk equivalent and are ignored.
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
                let Rect { x: ox, y: oy, .. } = desktop_bounds();
                let origin = (ox, oy);
                let at = CGEvent::location(Some(ev));
                let (x, y) = (to_local(at.x as i32, origin.0), to_local(at.y as i32, origin.1));
                // Far from every edge the engine has no use for a move: skip the send.
                if ctx.tx.wants_move(dx, dy, x, y, ctx.swallow.load(Ordering::Relaxed)) {
                    send(Captured::Moved { dx, dy, x, y });
                }
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
                    -field(ev, CGEventField::ScrollWheelEventDeltaAxis2) * i64::from(UNITS_PER_NOTCH),
                    field(ev, CGEventField::ScrollWheelEventDeltaAxis1) * i64::from(UNITS_PER_NOTCH),
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
        // A Dock swipe shares type 30 with magnify; AppKit would decode it as a zero magnify.
        CGEventType(30) if is_dock_swipe(ty, ev) => {
            let progress = CGEvent::double_value_field(Some(ev), DOCK_PROGRESS);
            let sample = DockSample {
                motion: field(ev, DOCK_MOTION),
                phase: field(ev, DOCK_PHASE),
                progress,
            };
            if let Some(gesture) = ctx.dock.borrow_mut().feed(sample) {
                send(Captured::Gesture {
                    gesture,
                    phase: Phase::None,
                });
            }
            None
        }
        CGEventType(18) | CGEventType(29) | CGEventType(30) | CGEventType(31) | CGEventType(32) => {
            gesture(ev, |gesture, phase| send(Captured::Gesture { gesture, phase }));
            None
        }
        _ => None,
    }
}

/// Reads a trackpad gesture through the public AppKit accessors (`+[NSEvent
/// eventWithCGEvent:]`, `magnification`, `rotation`, `phase`) and hands it to `out`.
fn gesture(ev: &CGEvent, out: impl FnOnce(Gesture, Phase)) {
    let Some(ns) = NSEvent::eventWithCGEvent(ev) else {
        return;
    };
    // `magnification` is a fraction (0.01 = 1%), `rotation` degrees, counter-clockwise positive.
    let g = match ns.r#type() {
        NSEventType::Magnify => Gesture::Magnify {
            milli: (ns.magnification() * 1000.0).round() as i32,
        },
        NSEventType::Rotate => Gesture::Rotate {
            millideg: (f64::from(ns.rotation()) * 1000.0).round() as i32,
        },
        NSEventType::SmartMagnify => Gesture::SmartZoom,
        _ => return,
    };
    let p = ns.phase();
    let phase = if p.contains(NSEventPhase::Began) {
        Phase::Began
    } else if p.contains(NSEventPhase::Ended) {
        Phase::Ended
    } else if p.contains(NSEventPhase::Cancelled) {
        Phase::Cancelled
    } else if p.contains(NSEventPhase::Changed) {
        Phase::Changed
    } else {
        Phase::None
    };
    out(g, phase);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tap context with an empty port, delivering into a channel.
    fn ctx(swallowing: bool) -> (TapCtx, std::sync::mpsc::Receiver<Captured>) {
        let (tx, rx) = Sink::channel();
        let ctx = TapCtx {
            tx,
            swallow: Arc::new(AtomicBool::new(swallowing)),
            port: OnceCell::new(),
            local: RefCell::new(LocalDowns::default()),
            sub: RefCell::new(Subpixel::default()),
            chord: RefCell::new(EscapeChord::default()),
            detached: Cell::new(false),
            dock: RefCell::new(DockSwipe::default()),
            probe: false,
        };
        (ctx, rx)
    }

    #[test]
    fn a_disabled_tap_forgets_held_keys_and_hands_swallowed_input_back() {
        let (ctx, rx) = ctx(true);
        // Ctrl+Alt+Cmd held, then the tap goes deaf and misses their release.
        for code in [0xE0, 0xE2, 0xE3] {
            ctx.chord.borrow_mut().key(code, true);
        }
        ctx.local.borrow_mut().deliver_locally(Held::Key(4), true, false);
        recover_after_disable(&ctx);
        assert!(!ctx.swallow.load(Ordering::Relaxed));
        assert_eq!(rx.try_recv(), Ok(Captured::Escape));
        // The chord starts from nothing: Esc alone is not the chord any more.
        assert!(!ctx.chord.borrow_mut().key(0x29, true));
        // `Held::Key(4)` was forgotten, so its release while swallowing is not local.
        assert!(!ctx.local.borrow_mut().deliver_locally(Held::Key(4), false, true));
        // Not swallowing: nothing to announce.
        let (ctx, rx) = self::ctx(false);
        recover_after_disable(&ctx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unmapped_releases_always_pass_but_presses_do_not_while_swallowing() {
        assert!(passes_unmapped(CGEventType::KeyUp, true));
        assert!(passes_unmapped(CGEventType::OtherMouseUp, true));
        assert!(!passes_unmapped(CGEventType::KeyDown, true));
        assert!(!passes_unmapped(CGEventType::MouseMoved, true));
        assert!(passes_unmapped(CGEventType::KeyDown, false));
    }
}
