// SPDX-License-Identifier: AGPL-3.0-only
//! Message-only windows and the message loop of a helper thread (capture, clipboard watch).

use std::ptr::{null, null_mut};
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, PostThreadMessageW,
    RegisterClassW, TranslateMessage, WM_QUIT, WNDCLASSW,
};

use super::wide;

/// Destroys a window when dropped; must drop on the thread that created it.
pub(super) struct WindowGuard(pub(super) HWND);

impl Drop for WindowGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the window was created on this thread and is destroyed once.
            unsafe { DestroyWindow(self.0) };
        }
    }
}

/// Registers window class `name` with window procedure `proc_` and creates a message-only
/// window of it on this thread. The guard holds a null handle if creation failed.
pub(super) fn message_window(
    name: &str,
    proc_: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) -> WindowGuard {
    // The class name doubles as the window title. Registering a class that already exists (a
    // restarted thread) fails harmlessly: the earlier registration has the same window procedure.
    let class = wide(name);
    // SAFETY: Win32 setup on this thread; the class name outlives the window.
    let instance = unsafe { GetModuleHandleW(null()) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(proc_),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        // SAFETY: WNDCLASSW is plain data for which all-zero bytes are valid.
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: `wc` is fully initialised and its class name outlives the call.
    unsafe { RegisterClassW(&wc) };
    // SAFETY: the class was registered above; a message-only parent needs no other state.
    WindowGuard(unsafe {
        CreateWindowExW(
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
        )
    })
}

/// Pumps this thread's messages until `WM_QUIT`. `taken` sees each message first and returns
/// `true` to consume it; the rest are dispatched to their windows.
pub(super) fn pump(mut taken: impl FnMut(&MSG) -> bool) {
    // SAFETY: MSG is plain data for which all-zero bytes are valid.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: standard message loop on the thread that owns the hooks and windows.
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        if taken(&msg) {
            continue;
        }
        // SAFETY: `msg` was just filled in by `GetMessageW`.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Ends the message loop of `thread` (whose Win32 id is `id`) and waits for it.
pub(super) fn quit_thread(thread: JoinHandle<()>, id: u32) {
    // A thread that already ended (a panic) has an id Windows may give to another thread;
    // only post to a live one.
    if !thread.is_finished() {
        // SAFETY: posting WM_QUIT to one of our own threads ends its message loop.
        unsafe { PostThreadMessageW(id, WM_QUIT, 0, 0) };
    }
    let _ = thread.join();
}
