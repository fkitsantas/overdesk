// SPDX-License-Identifier: AGPL-3.0-only
//! Posting input as if from Apple hardware: pointer, buttons, keys, scroll and gestures.

use std::time::Instant;

use objc2_app_kit::NSEvent;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};
use overdesk_proto::{Button, Gesture, Phase, ScrollUnit};

use super::display::{DisplayWatch, desktop_bounds};
use super::{INJECTED_TAG, MacError, permissions};
use crate::input::{Notches, Pressed, Rect};
use crate::mac::events::{ClickCounter, MoveKind, flags_for, move_kind, phase_to_cg};
use crate::mac::keymap::code_to_vk;

/// Fallback double-click interval when the system value is unreadable.
const DEFAULT_DOUBLE_CLICK_MS: u64 = 500;

/// The user's double-click interval in ms (System Settings › Mouse).
fn double_click_ms() -> u64 {
    let s = NSEvent::doubleClickInterval();
    if s.is_finite() && s > 0.0 {
        (s * 1000.0) as u64
    } else {
        DEFAULT_DOUBLE_CLICK_MS
    }
}

/// `kCGEventFlagMaskAlphaShift`: Caps Lock state, owned by the system, never by us.
const FLAG_CAPS_LOCK: u64 = 0x0001_0000;

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

/// Builds a synthetic trackpad gesture event.
///
/// PRIVATE, UNDOCUMENTED API (like the CGS calls above): macOS has no public way to post
/// a pinch or rotate, so this writes the event type (`kCGSEventGesture`, 29) and the
/// gesture fields that WindowServer itself uses. It rules out the Mac App Store and may
/// break in any future macOS release; the only effect then is that gestures do nothing.
/// The numbers come from Apple's open-source WebKit test SPI header
/// (Tools/TestRunnerShared/spi/CoreGraphicsTestSPI.h: `kCGSEventGesture` 29,
/// `kCGEventGestureHIDType` 110, `kCGEventGestureZoomValue` 113,
/// `kCGEventGestureRotationValue` 114, `kCGEventGesturePhase` 132), WebKitTestRunner's
/// use of HID type 8 (`kIOHIDEventTypeZoom`), IOKit's `IOHIDEventTypes.h` (rotation = 5) and
/// the public `CGGesturePhase` values in CoreGraphics' CGEventTypes.h.
fn private_gesture_event(source: &CGEventSource, gesture: Gesture, phase: Phase) -> Option<CFRetained<CGEvent>> {
    const GESTURE_TYPE: u32 = 29; // kCGSEventGesture
    const HID_TYPE: CGEventField = CGEventField(110);
    const ZOOM_VALUE: CGEventField = CGEventField(113);
    const ROTATION_VALUE: CGEventField = CGEventField(114);
    const PHASE: CGEventField = CGEventField(132);
    let ev = CGEvent::new(Some(source))?;
    CGEvent::set_type(Some(&ev), CGEventType(GESTURE_TYPE));
    let (hid, value_field, value) = match gesture {
        Gesture::Magnify { milli } => (8, ZOOM_VALUE, f64::from(milli) / 1000.0),
        Gesture::Rotate { millideg } => (5, ROTATION_VALUE, f64::from(millideg) / 1000.0),
        Gesture::SmartZoom => return None, // shortcut: no known event shape for smart zoom; add when found
    };
    CGEvent::set_integer_value_field(Some(&ev), HID_TYPE, hid);
    CGEvent::set_double_value_field(Some(&ev), value_field, value);
    // Same bit values as the public CGGesturePhase scroll phases.
    CGEvent::set_integer_value_field(Some(&ev), PHASE, phase_to_cg(phase).0);
    Some(ev)
}

/// Posts input that apps cannot tell apart from real Apple hardware.
pub struct Injector {
    /// `kCGEventSourceStateHIDSystemState` source, so posted events update the same
    /// global key and button state as real hardware.
    source: CFRetained<CGEventSource>,
    /// Overdesk codes this injector holds down, in press order; they decide the
    /// modifier flags of every posted event.
    keys: Pressed<u32>,
    /// Buttons this injector holds down; they make moves into drags.
    buttons: Pressed<Button>,
    /// Computes the multi-click count of each press.
    clicks: ClickCounter,
    /// Click count of the latest press, also stamped on its release as macOS expects.
    last_click_count: i64,
    /// Last position this injector moved the pointer to (global points); presses and
    /// releases are posted here.
    pos: CGPoint,
    /// Magnify or rotate gesture begun but not yet ended; `release_all` ends it.
    open_gesture: Option<Gesture>,
    /// Time origin for the click counter's millisecond timestamps.
    epoch: Instant,
    /// Double-click interval in ms, read once at creation.
    double_click_ms: u64,
    /// Sub-notch wheel remainders carried between scroll events.
    notches: Notches,
    /// Keeps the desktop bounds live for the injector's lifetime.
    _watch: DisplayWatch,
}

impl Injector {
    /// Creates an injector, starting from wherever the cursor is now. Fails without
    /// Accessibility permission, since posted events would be silently dropped.
    pub fn new() -> Result<Self, MacError> {
        if !permissions().1 {
            return Err(MacError::NoPostPermission);
        }
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok_or(MacError::SourceCreateFailed)?;
        // A fresh null event carries the current cursor location.
        let pos = CGEvent::new(None)
            .map(|e| CGEvent::location(Some(&e)))
            .unwrap_or(CGPoint { x: 0.0, y: 0.0 });
        Ok(Self {
            source,
            keys: Pressed::default(),
            buttons: Pressed::default(),
            clicks: ClickCounter::default(),
            last_click_count: 1,
            pos,
            open_gesture: None,
            epoch: Instant::now(),
            double_click_ms: double_click_ms(),
            notches: Notches::default(),
            _watch: DisplayWatch::new(),
        })
    }

    /// Tags `ev` as ours, gives it the flags of the held modifiers (keeping the system's
    /// Caps Lock state) and posts it at the HID level, where real hardware events enter.
    fn post(&self, ev: &CGEvent) {
        CGEvent::set_integer_value_field(Some(ev), CGEventField::EventSourceUserData, INJECTED_TAG);
        let caps = CGEvent::flags(Some(ev)).0 & FLAG_CAPS_LOCK;
        CGEvent::set_flags(Some(ev), CGEventFlags(flags_for(self.keys.as_slice()) | caps));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(ev));
    }

    /// Moves the pointer to desktop-box coordinates (points, top-left origin).
    pub fn pointer(&mut self, x: f64, y: f64) {
        let Rect { x: ox, y: oy, .. } = desktop_bounds();
        let (x, y) = (x + f64::from(ox), y + f64::from(oy));
        let (dx, dy) = (x - self.pos.x, y - self.pos.y);
        self.pos = CGPoint { x, y };
        let (ty, button) = match move_kind(self.buttons.as_slice()) {
            MoveKind::Moved => (CGEventType::MouseMoved, CGMouseButton::Left),
            MoveKind::LeftDragged => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            MoveKind::RightDragged => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            MoveKind::OtherDragged => {
                let held = self
                    .buttons
                    .as_slice()
                    .iter()
                    .find(|b| !matches!(b, Button::Left | Button::Right));
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

    /// Where the system cursor actually is now (desktop-box points).
    pub fn cursor() -> (f64, f64) {
        let p = CGEvent::new(None)
            .map(|e| CGEvent::location(Some(&e)))
            .unwrap_or(CGPoint { x: 0.0, y: 0.0 });
        let Rect { x: ox, y: oy, .. } = desktop_bounds();
        (p.x - f64::from(ox), p.y - f64::from(oy))
    }

    /// Releases every key and button this injector holds down and ends any gesture left
    /// open. Call when focus leaves this Mac or the peer driving it disconnects.
    pub fn release_all(&mut self) {
        if let Some(g) = self.open_gesture {
            self.gesture(g, Phase::Ended);
        }
        for code in self.keys.snapshot().into_iter().rev() {
            self.key(code, false);
        }
        for button in self.buttons.snapshot() {
            self.button(button, false);
        }
    }

    /// Presses or releases `button` at the injector's pointer position, with the
    /// click count and button number real hardware would report.
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
                    .press(button, t_ms, self.pos.x as i32, self.pos.y as i32, self.double_click_ms);
        }
        self.buttons.set(button, pressed);
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
        // Update held state first: a modifier's own event carries its new flag state.
        let repeat = self.keys.set(code, pressed) && pressed;
        match CGEvent::new_keyboard_event(Some(&self.source), vk, pressed) {
            Some(ev) => {
                CGEvent::set_integer_value_field(Some(&ev), CGEventField::KeyboardEventAutorepeat, repeat as i64);
                self.post(&ev);
                true
            }
            None => false,
        }
    }

    /// Posts a trackpad magnify, rotate or smart-zoom gesture event (see `private_gesture_event`),
    /// remembering an unfinished gesture so [`Self::release_all`] can end it.
    pub fn gesture(&mut self, gesture: Gesture, phase: Phase) {
        self.open_gesture = matches!(phase, Phase::Began | Phase::Changed | Phase::MayBegin)
            .then_some(gesture)
            .filter(|g| !matches!(g, Gesture::SmartZoom));
        if let Some(ev) = private_gesture_event(&self.source, gesture, phase) {
            self.post(&ev);
        }
    }

    /// Posts one scroll event with its gesture/momentum phase, so trackpad scrolling
    /// with inertia and rubber-banding looks native. Pixel deltas are marked continuous
    /// like trackpad input; notch deltas become line scrolls like a wheel mouse.
    pub fn scroll(&mut self, dx: i32, dy: i32, unit: ScrollUnit, phase: Phase) {
        let (cg_unit, dx, dy) = match unit {
            ScrollUnit::Pixel => (CGScrollEventUnit::Pixel, dx, dy),
            ScrollUnit::Notch120 => {
                let (nx, ny) = self.notches.feed(dx, dy, ScrollUnit::Notch120);
                (CGScrollEventUnit::Line, nx, ny)
            }
        };
        if cg_unit == CGScrollEventUnit::Line && (dx, dy) == (0, 0) {
            return;
        }
        // Two axes: axis 1 vertical, axis 2 horizontal with Quartz's positive-left sign.
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

impl Drop for Injector {
    /// Nothing stays held down once the injector goes away.
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use objc2_app_kit::{NSEventPhase, NSEventType};

    use super::*;

    /// AppKit must read our synthetic events back as the same gesture.
    #[test]
    fn gesture_roundtrip() {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).unwrap();
        for (g, ty) in [
            (Gesture::Magnify { milli: 123 }, NSEventType::Magnify),
            (Gesture::Rotate { millideg: -4500 }, NSEventType::Rotate),
        ] {
            let ev = private_gesture_event(&source, g, Phase::Changed).unwrap();
            let ns = NSEvent::eventWithCGEvent(&ev).unwrap();
            assert_eq!(ns.r#type(), ty);
            assert!(ns.phase().contains(NSEventPhase::Changed));
            match g {
                Gesture::Magnify { .. } => assert!((ns.magnification() - 0.123).abs() < 1e-6),
                _ => assert!((f64::from(ns.rotation()) + 4.5).abs() < 1e-3, "{}", ns.rotation()),
            }
        }
    }
}
