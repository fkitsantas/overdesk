// SPDX-License-Identifier: AGPL-3.0-only
//! The clipboard: change notification through a message-only window, the change counter, and
//! the password-manager privacy markers.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;
use std::sync::mpsc::{Sender, channel};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, WM_CLIPBOARDUPDATE};

use super::wide;
use super::window::{message_window, pump, quit_thread};
use crate::win::events::marker_is_private;

/// The clipboard sequence number: it increments on every clipboard change.
pub fn clipboard_change_count() -> i64 {
    // SAFETY: plain Win32 call with no arguments.
    i64::from(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}

thread_local! {
    /// The clipboard watcher thread's callback; `None` on every other thread.
    static ON_CLIPBOARD: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Window procedure of the message-only window that listens for clipboard changes.
///
/// # Safety
///
/// Called only by Windows, on the watcher thread that created the window, with message
/// parameters matching `msg`.
unsafe extern "system" fn clipboard_window(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        // The callback is the app's code: a panic in it must not unwind into Windows.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            ON_CLIPBOARD.with_borrow(|f| f.iter().for_each(|f| f()));
        }));
        return 0;
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Body of the clipboard watcher thread: a message-only window registered with
/// `AddClipboardFormatListener`, pumping messages until `WM_QUIT`. Reports its thread id on
/// `ready`, or `None` if the window or listener could not be created. The window is
/// destroyed on every path out.
fn run_clipboard_watch(on_change: Box<dyn Fn() + Send>, ready: Sender<Option<u32>>) {
    ON_CLIPBOARD.with_borrow_mut(|f| *f = Some(on_change));
    let window = message_window("OverdeskClipboard", clipboard_window);
    // SAFETY: the window is live and owned by this thread.
    if window.0.is_null() || unsafe { AddClipboardFormatListener(window.0) } == 0 {
        let _ = ready.send(None);
        ON_CLIPBOARD.with_borrow_mut(|f| *f = None);
        return;
    }
    // SAFETY: plain Win32 call.
    let _ = ready.send(Some(unsafe { GetCurrentThreadId() }));
    pump(|_| false);
    // SAFETY: registered above on this window, removed once, before the guard destroys it.
    unsafe { RemoveClipboardFormatListener(window.0) };
    drop(window);
    ON_CLIPBOARD.with_borrow_mut(|f| *f = None);
}

/// Calls `on_change` on the watcher thread (not the caller's) each time the clipboard
/// changes (`WM_CLIPBOARDUPDATE`), so it must be quick and must not block. `None` if the
/// listener could not be set up. The returned function stops the thread.
pub fn watch_clipboard(on_change: Box<dyn Fn() + Send>) -> Option<Box<dyn FnOnce() + Send>> {
    let (ready_tx, ready_rx) = channel();
    let thread = std::thread::Builder::new()
        .name("overdesk-win-clipboard".into())
        .spawn(move || run_clipboard_watch(on_change, ready_tx))
        .ok()?;
    let thread_id = ready_rx.recv().ok()??;
    Some(Box::new(move || quit_thread(thread, thread_id)))
}

/// Reads the DWORD payload of clipboard `format`. `None` if the clipboard cannot be
/// opened, the format has no data, or the data is shorter than four bytes.
fn clipboard_dword(format: u32) -> Option<u32> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    // SAFETY: the clipboard is open between OpenClipboard and CloseClipboard, which keeps
    // the HGLOBAL from GetClipboardData valid; it is locked only while the DWORD is copied
    // out, and only after GlobalSize confirms at least four bytes.
    unsafe {
        if OpenClipboard(null_mut()) == 0 {
            return None;
        }
        let mut value = None;
        let handle = GetClipboardData(format);
        if !handle.is_null() && GlobalSize(handle) >= 4 {
            let p = GlobalLock(handle) as *const u32;
            if !p.is_null() {
                value = Some(p.read_unaligned());
                GlobalUnlock(handle);
            }
        }
        CloseClipboard();
        value
    }
}

/// True when the clipboard carries a marker that password managers set on secrets so
/// clipboard tools skip them. Per Microsoft's clipboard-history docs,
/// `ExcludeClipboardContentFromMonitorProcessing` excludes by presence alone, while
/// `CanIncludeInClipboardHistory` and `CanUploadToCloudClipboard` hold a DWORD where 0
/// forbids and 1 allows. A value that cannot be read counts as private.
pub fn clipboard_is_private() -> bool {
    use windows_sys::Win32::System::DataExchange::{IsClipboardFormatAvailable, RegisterClipboardFormatW};
    /// Marker format name, and whether its DWORD (rather than presence) decides.
    const MARKERS: [(&str, bool); 3] = [
        ("ExcludeClipboardContentFromMonitorProcessing", false),
        ("CanIncludeInClipboardHistory", true),
        ("CanUploadToCloudClipboard", true),
    ];
    MARKERS.iter().any(|&(name, value_matters)| {
        let wide = wide(name);
        // SAFETY: `wide` is NUL-terminated and outlives the call; neither function needs
        // the clipboard to be open.
        let format = unsafe { RegisterClipboardFormatW(wide.as_ptr()) };
        if format == 0 || unsafe { IsClipboardFormatAvailable(format) } == 0 {
            return false;
        }
        marker_is_private(value_matters, value_matters.then(|| clipboard_dword(format)).flatten())
    })
}
