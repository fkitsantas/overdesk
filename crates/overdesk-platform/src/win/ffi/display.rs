// SPDX-License-Identifier: AGPL-3.0-only
//! The virtual screen and the process's DPI awareness: every capture and injection coordinate
//! is relative to the screen's corner.

use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};
use windows_sys::core::BOOL;

use crate::input::{Rect, Stamped, Warning, box_relative, to_local};

/// Makes this process report coordinates in real pixels, not DPI-virtualised ones.
/// Process-wide, set once; a refusal is returned by the first call only.
///
/// Windows refuses the change once a window exists in the process (and when the manifest
/// already fixed an awareness), so an app must call this as early as it can, before it
/// creates any window; starting capture or injection calls it too, but may be too late then.
pub fn ensure_dpi_aware() -> Option<Warning> {
    static ONCE: Once = Once::new();
    let mut refused = None;
    ONCE.call_once(|| {
        // SAFETY: plain Win32 call with a documented constant.
        if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } == 0 {
            refused = Some(Warning::DpiAwarenessRefused);
        }
    });
    refused
}

/// How long a [`virtual_screen`] reading is trusted. Message-only windows get no
/// `WM_DISPLAYCHANGE` broadcast, so this bounds how long a hot-plug can go unnoticed.
const VIRTUAL_TTL: Duration = Duration::from_secs(1);

/// The last [`virtual_screen`] reading.
pub(super) static VIRTUAL: Mutex<Stamped<Rect>> = Mutex::new(Stamped::new());

/// The virtual screen (bounding box of all monitors) in global pixels; its origin is
/// negative when a monitor sits left of or above the primary one. Cached for
/// [`VIRTUAL_TTL`] (and dropped on `WM_DISPLAYCHANGE`), so the motion path does not ask the
/// OS four questions per event while monitor hot-plug is still picked up.
pub(super) fn virtual_screen() -> Rect {
    let _ = ensure_dpi_aware(); // a refusal is reported by the first caller: the app or daemon startup
    let now = Instant::now();
    if let Some(r) = VIRTUAL.lock().ok().and_then(|c| c.get(now, VIRTUAL_TTL)) {
        return r;
    }
    // SAFETY: plain Win32 calls.
    let r = unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN) as u32,
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN) as u32,
        }
    };
    if let Ok(mut c) = VIRTUAL.lock() {
        c.set(r, now);
    }
    r
}

/// Desktop size in pixels: the virtual screen spanning all monitors, this PC's screen on
/// the Desk.
pub fn main_display_size() -> (u32, u32) {
    let Rect { w, h, .. } = virtual_screen();
    (w, h)
}

/// Each monitor's rectangle in pixels, relative to the virtual screen (see [`main_display_size`]).
pub fn monitors() -> Vec<Rect> {
    let _ = ensure_dpi_aware(); // see `virtual_screen`
    /// `EnumDisplayMonitors` callback: appends the monitor's bounds to the `Vec<Rect>` in `data`.
    unsafe extern "system" fn each(_: HMONITOR, _: HDC, r: *mut RECT, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the `&mut Vec<Rect>` passed below, alive for the whole call; `r` is valid.
        let (all, r) = unsafe { (&mut *(data as *mut Vec<Rect>), &*r) };
        all.push(Rect {
            x: r.left,
            y: r.top,
            w: (r.right - r.left) as u32,
            h: (r.bottom - r.top) as u32,
        });
        1
    }
    let mut all: Vec<Rect> = Vec::new();
    // SAFETY: the callback only runs during this call and `all` outlives it.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(each),
            &mut all as *mut _ as LPARAM,
        )
    };
    box_relative(&all)
}

/// The system cursor in virtual-screen-relative pixels.
pub(super) fn cursor_local() -> (i32, i32) {
    let mut at = POINT { x: 0, y: 0 };
    // SAFETY: writes one POINT.
    unsafe { GetCursorPos(&mut at) };
    let Rect { x: ox, y: oy, .. } = virtual_screen();
    (to_local(at.x, ox), to_local(at.y, oy))
}
