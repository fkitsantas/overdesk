// SPDX-License-Identifier: AGPL-3.0-only
//! The desktop's displays: the bounding box every capture and injection coordinate is relative
//! to, cached while a [`DisplayWatch`] is alive.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use crate::input::{Rect, bounding_box, box_relative};

/// Bounding box of every active display in global points (main display at 0,0, y down;
/// origins may be negative). Falls back to the main display if the list is unavailable.
///
/// Cached while a [`DisplayWatch`] is alive (the watch invalidates it on display
/// changes, and a reading older than [`BOUNDS_TTL`] is not trusted, since the callback
/// only runs while some thread services the main run loop); computed afresh otherwise.
pub(super) fn desktop_bounds() -> Rect {
    let now = Instant::now();
    let cached = DISPLAYS.lock().unwrap_or_else(PoisonError::into_inner).fresh(now);
    if let Some(bounds) = cached {
        return bounds;
    }
    // CoreGraphics is called without the lock held: the reconfiguration callback takes it.
    let bounds = read_desktop_bounds();
    let mut st = DISPLAYS.lock().unwrap_or_else(PoisonError::into_inner);
    if st.watchers > 0 {
        st.bounds = Some((bounds, now));
    }
    bounds
}

/// Queries CoreGraphics for [`desktop_bounds`].
fn read_desktop_bounds() -> Rect {
    let displays = read_displays();
    bounding_box(displays.iter().copied()).unwrap_or_else(main_display_rect)
}

/// The main display's bounds, for when the active list is unavailable.
fn main_display_rect() -> Rect {
    display_rect(objc2_core_graphics::CGMainDisplayID())
}

/// One display's bounds in global points.
fn display_rect(id: objc2_core_graphics::CGDirectDisplayID) -> Rect {
    let r = objc2_core_graphics::CGDisplayBounds(id);
    Rect {
        x: r.origin.x as i32,
        y: r.origin.y as i32,
        w: r.size.width as u32,
        h: r.size.height as u32,
    }
}

/// Every active display's bounds in global points; empty when CoreGraphics reports none.
fn read_displays() -> Vec<Rect> {
    use objc2_core_graphics::{CGDirectDisplayID, CGGetActiveDisplayList};
    const MAX: usize = 32;
    let mut ids: [CGDirectDisplayID; MAX] = [0; MAX];
    let mut n = 0u32;
    // SAFETY: `ids` has room for `MAX` entries and `n` is a valid out-pointer.
    let ok = unsafe { CGGetActiveDisplayList(MAX as u32, ids.as_mut_ptr(), &mut n) }.0 == 0;
    let ids = if ok { &ids[..(n as usize).min(MAX)] } else { &[] };
    ids.iter().map(|&id| display_rect(id)).collect()
}

/// Display-watch bookkeeping shared with the reconfiguration callback.
struct DisplayState {
    /// Live [`DisplayWatch`] count; the callback is registered while it is non-zero.
    watchers: usize,
    /// Last [`read_desktop_bounds`] result and when it was read; `None` after a display change.
    bounds: Option<(Rect, Instant)>,
}

/// How long a cached bounds reading is trusted at most, as a safety net for a reconfiguration
/// callback that never fires.
const BOUNDS_TTL: std::time::Duration = std::time::Duration::from_secs(1);

impl DisplayState {
    /// The cached bounds if a watch is alive and the reading is younger than [`BOUNDS_TTL`] at `now`.
    fn fresh(&self, now: Instant) -> Option<Rect> {
        let (rect, at) = self.bounds?;
        (self.watchers > 0 && now.saturating_duration_since(at) < BOUNDS_TTL).then_some(rect)
    }
}

/// See [`DisplayState`].
static DISPLAYS: Mutex<DisplayState> = Mutex::new(DisplayState {
    watchers: 0,
    bounds: None,
});

/// Called by CoreGraphics on display changes; drops the cached bounds once the
/// reconfiguration has finished (the `Begin` call precedes the change).
extern "C-unwind" fn display_changed(
    _display: objc2_core_graphics::CGDirectDisplayID,
    flags: objc2_core_graphics::CGDisplayChangeSummaryFlags,
    _info: *mut c_void,
) {
    if !flags.contains(objc2_core_graphics::CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        DISPLAYS.lock().unwrap_or_else(PoisonError::into_inner).bounds = None;
    }
}

/// Keeps [`desktop_bounds`] live: while any exists, display changes refresh the bounds.
pub(super) struct DisplayWatch;

impl DisplayWatch {
    /// Registers the callback if this is the first watch.
    pub(super) fn new() -> Self {
        let first = {
            let mut st = DISPLAYS.lock().unwrap_or_else(PoisonError::into_inner);
            st.watchers += 1;
            st.bounds = None;
            st.watchers == 1
        };
        if first {
            // SAFETY: `display_changed` matches the callback signature and ignores `user_info` (null).
            // Called with the lock released, as the callback takes it.
            let _ = unsafe {
                objc2_core_graphics::CGDisplayRegisterReconfigurationCallback(Some(display_changed), null_mut())
            };
        }
        Self
    }
}

impl Drop for DisplayWatch {
    fn drop(&mut self) {
        let last = {
            let mut st = DISPLAYS.lock().unwrap_or_else(PoisonError::into_inner);
            st.watchers -= 1;
            st.watchers == 0
        };
        if last {
            // SAFETY: same callback and `user_info` as registered in `DisplayWatch::new`.
            let _ = unsafe {
                objc2_core_graphics::CGDisplayRemoveReconfigurationCallback(Some(display_changed), null_mut())
            };
        }
    }
}

/// Desktop size in points: the bounding box of all displays, this Mac's screen on the Desk.
pub fn main_display_size() -> (u32, u32) {
    let Rect { w, h, .. } = desktop_bounds();
    (w, h)
}

/// Each display's rectangle in points, relative to the desktop box (see [`main_display_size`]).
pub fn monitors() -> Vec<Rect> {
    box_relative(&read_displays())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_cache_expires_and_needs_a_watch() {
        let t0 = Instant::now();
        let rect = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        };
        let st = |watchers| DisplayState {
            watchers,
            bounds: Some((rect, t0)),
        };
        assert_eq!(st(1).fresh(t0 + BOUNDS_TTL / 2), Some(rect));
        assert_eq!(st(1).fresh(t0 + BOUNDS_TTL), None, "stale: re-read");
        assert_eq!(st(0).fresh(t0), None, "no watch, no cache");
        assert_eq!(
            DisplayState {
                watchers: 1,
                bounds: None
            }
            .fresh(t0),
            None
        );
    }
}
