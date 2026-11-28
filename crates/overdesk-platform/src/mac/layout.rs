// SPDX-License-Identifier: AGPL-3.0-only
//! Characters from the macOS keyboard layout: `UCKeyTranslate` over the layout data of the
//! current (or a named) keyboard input source, with the dead-key state carried across calls.
//!
//! Threading: the Text Input Sources API (TIS/TSM) may assert that it runs on the main
//! queue, but key lookups happen on a tokio worker. So the current layout's `uchr` bytes are
//! copied into a process-wide cache (`CURRENT`) by code that runs *on the main queue*
//! ([`refresh_current_layout`] at start and whenever translation turns on, and the
//! "selected keyboard input source changed" notification), and `UCKeyTranslate`, a pure
//! function over those bytes, runs on any thread. A process that never services the main
//! queue (a headless daemon blocked in `block_on` on the main thread) keeps `None` in the
//! cache: every lookup answers "no character" and the hub sends the plain `Key`.
#![allow(unsafe_code)] // Carbon FFI; each unsafe block states its invariant.

use std::ffi::{CStr, c_char, c_void};
use std::ptr::{copy_nonoverlapping, null, null_mut};
use std::sync::{Once, RwLock};

use super::keymap::code_to_vk;
use crate::text::{Layout, Mods, OsMods, Raw};

/// An opaque CoreFoundation object pointer.
type CfRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CfRef);
    fn CFDataGetBytePtr(data: CfRef) -> *const u8;
    fn CFDataGetLength(data: CfRef) -> isize;
    fn CFNotificationCenterGetDistributedCenter() -> CfRef;
    fn CFNotificationCenterAddObserver(
        center: CfRef,
        observer: *const c_void,
        callback: extern "C" fn(CfRef, *mut c_void, CfRef, *const c_void, CfRef),
        name: CfRef,
        object: *const c_void,
        suspension_behavior: isize,
    );
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    fn CFStringGetCString(s: CfRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyUnicodeKeyLayoutData: CfRef;
    static kTISPropertyInputSourceID: CfRef;
    static kTISNotifySelectedKeyboardInputSourceChanged: CfRef;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> CfRef;
    fn TISGetInputSourceProperty(source: CfRef, key: CfRef) -> CfRef;
    fn TISCreateInputSourceList(properties: CfRef, include_all_installed: u8) -> CfRef;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const u8,
        virtual_key: u16,
        key_action: u16,
        modifier_state: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        max_len: usize,
        actual_len: *mut usize,
        out: *mut u16,
    ) -> i32;
}

// `dispatch_get_main_queue()` is a header macro for the address of this libSystem symbol.
unsafe extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    /// 1 when the calling thread is the process's main thread.
    fn pthread_main_np() -> i32;
}

/// `CFNotificationSuspensionBehaviorDeliverImmediately`.
const DELIVER_IMMEDIATELY: isize = 4;
/// `kCGEventSourceStateCombinedSessionState`.
const COMBINED_SESSION_STATE: i32 = 0;
/// `kCFStringEncodingUTF8`.
const UTF8: u32 = 0x0800_0100;
/// `UCKeyTranslate` modifier bits (Carbon modifiers shifted right by 8).
const MOD_SHIFT: u32 = 1 << 1;
/// Caps Lock (`alphaLock`).
const MOD_CAPS: u32 = 1 << 2;
/// Option.
const MOD_OPTION: u32 = 1 << 3;

/// Which layout to read.
#[derive(Debug, Default, Clone)]
pub enum Source {
    /// Whatever keyboard layout is active at each key press.
    #[default]
    Current,
    /// A fixed input source such as `com.apple.keylayout.German`, which may be installed
    /// without being selected (the user's active layout is not touched).
    Id(String),
}

/// A copy of a `UCKeyboardLayout` (`uchr`) blob, in 8-byte words so the struct inside is
/// aligned however the system copy was.
type Uchr = Vec<u64>;

/// The current keyboard layout's data, filled on the main queue. See the module docs.
static CURRENT: RwLock<Option<Uchr>> = RwLock::new(None);

/// The least a `UCKeyboardLayout` can be: its 12-byte header and one `UCKeyboardTypeHeader`
/// (seven 32-bit fields). Shorter data cannot be a layout. This is only a floor for the fixed
/// part: the offsets inside are not checked, as the data comes from the OS, not from a peer.
const MIN_UCHR_LEN: usize = 12 + 28;

/// Copies `bytes` into 8-byte words; `None` if they are too short to be a `uchr`.
fn aligned_copy(bytes: &[u8]) -> Option<Uchr> {
    if bytes.len() < MIN_UCHR_LEN {
        return None;
    }
    let mut out = vec![0u64; bytes.len().div_ceil(8)];
    // SAFETY: `out` has room for `bytes.len()` bytes (words rounded up) and cannot overlap `bytes`.
    unsafe { copy_nonoverlapping(bytes.as_ptr(), out.as_mut_ptr().cast::<u8>(), bytes.len()) };
    Some(out)
}

/// Copies the bytes of a `CFData` holding a `uchr`; `None` if it is too short to be one.
///
/// # Safety
///
/// `data` must be a live CFData.
unsafe fn copy_uchr(data: CfRef) -> Option<Uchr> {
    // SAFETY: the pointer is valid for the CFData's length while it lives, and the slice is
    // dropped before the CFData can be.
    let bytes = unsafe {
        let len = usize::try_from(CFDataGetLength(data)).unwrap_or(0);
        let ptr = CFDataGetBytePtr(data);
        if ptr.is_null() {
            return None;
        }
        std::slice::from_raw_parts(ptr, len)
    };
    aligned_copy(bytes)
}

/// The layout data of `src` (borrowed), copied; `None` if it has none (an input method).
///
/// # Safety
///
/// `src` must be null or a live `TISInputSourceRef`.
unsafe fn uchr_of(src: CfRef) -> Option<Uchr> {
    if src.is_null() {
        return None;
    }
    // SAFETY: `src` is live per the contract; the returned CFData is borrowed from it.
    unsafe {
        let data = TISGetInputSourceProperty(src, kTISPropertyUnicodeKeyLayoutData);
        if data.is_null() { None } else { copy_uchr(data) }
    }
}

/// Reads the active keyboard layout from TIS. Call on the main queue only (tests excepted).
fn read_current() -> Option<Uchr> {
    // SAFETY: the Copy'd source is ours until released, once, after its data is copied.
    unsafe {
        let src = TISCopyCurrentKeyboardLayoutInputSource();
        let out = uchr_of(src);
        if !src.is_null() {
            CFRelease(src);
        }
        out
    }
}

/// Reads the layout of the installed input source `id` from TIS (it need not be selected).
fn read_named(id: &str) -> Option<Uchr> {
    // SAFETY: the created list owns the source `find` borrows; both are used before the one release.
    unsafe {
        let list = TISCreateInputSourceList(null(), 1);
        if list.is_null() {
            return None;
        }
        let out = uchr_of(find(list, id));
        CFRelease(list);
        out
    }
}

/// Stores the active layout into [`CURRENT`]. Main queue only.
fn store_current() {
    let fresh = read_current();
    if let Ok(mut cur) = CURRENT.write() {
        *cur = fresh;
    }
}

/// Runs on the main queue: fills the cache and, once, subscribes to layout changes.
extern "C" fn on_main(_: *mut c_void) {
    static SUBSCRIBE: Once = Once::new();
    store_current();
    SUBSCRIBE.call_once(|| {
        // SAFETY: runs on the main thread, whose run loop delivers distributed notifications.
        // The observer token is only an identity (never dereferenced); the callback has the
        // CFNotificationCallback signature and, with no data of its own, lives forever.
        unsafe {
            CFNotificationCenterAddObserver(
                CFNotificationCenterGetDistributedCenter(),
                (&raw const CURRENT).cast(),
                layout_changed,
                kTISNotifySelectedKeyboardInputSourceChanged,
                null(),
                DELIVER_IMMEDIATELY,
            );
        }
    });
}

/// The notification callback; delivered on the main thread that subscribed.
extern "C" fn layout_changed(_: CfRef, _: *mut c_void, _: CfRef, _: *const c_void, _: CfRef) {
    store_current();
}

/// Asks the main queue to (re)read the active layout into the cache and to watch for layout
/// changes. Returns at once; if the main queue is never serviced nothing happens (see the
/// module docs). Safe from any thread.
pub fn refresh_current_layout() {
    // SAFETY: the main queue lives for the whole process; `on_main` takes no context.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, null_mut(), on_main) };
}

/// How long [`MacLayout::new`] waits for the main queue to read a named layout. A process that
/// never services its main queue gets no layout instead of a hang.
const MAIN_QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Runs `work` once on the main queue. Returns at once; if the main queue is never serviced
/// it never runs.
fn run_on_main(work: impl FnOnce() + Send + 'static) {
    extern "C" fn trampoline(context: *mut c_void) {
        // SAFETY: `context` is the `Box<Box<dyn FnOnce() + Send>>` leaked below, run exactly once.
        let work = unsafe { Box::from_raw(context.cast::<Box<dyn FnOnce() + Send>>()) };
        work();
    }
    let context = Box::into_raw(Box::new(Box::new(work) as Box<dyn FnOnce() + Send>));
    // SAFETY: the main queue lives for the whole process and `trampoline` takes ownership of `context`.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, context.cast(), trampoline) };
}

/// Reads the named layout on the main thread, from any thread: directly when already there,
/// else by dispatching to the main queue and waiting up to [`MAIN_QUEUE_WAIT`].
fn read_named_on_main(id: &str) -> Option<Uchr> {
    // SAFETY: a plain libSystem query with no arguments.
    if unsafe { pthread_main_np() } == 1 {
        return read_named(id);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let id = id.to_owned();
    run_on_main(move || {
        let _ = tx.send(read_named(&id));
    });
    rx.recv_timeout(MAIN_QUEUE_WAIT).ok().flatten()
}

/// The macOS layout lookup.
#[derive(Debug)]
pub struct MacLayout {
    /// A fixed source's layout (read once at construction); `None` for the current layout.
    fixed: Option<Uchr>,
    /// Whether this lookup follows the current layout (the shared cache) rather than `fixed`.
    follows_current: bool,
    /// `UCKeyTranslate`'s dead-key state; 0 when no dead key is pending.
    dead: u32,
}

impl Default for MacLayout {
    /// Follows the current layout.
    fn default() -> Self {
        Self {
            fixed: None,
            follows_current: true,
            dead: 0,
        }
    }
}

impl MacLayout {
    /// A lookup over `source`. A named source is read from TIS here and now, on the main
    /// thread (this call hops there and waits, for up to a second, when made elsewhere; a
    /// process that never services its main queue gets a lookup with no layout, which answers
    /// "no character"); the current layout is read lazily from the main-queue cache.
    pub fn new(source: Source) -> Self {
        match source {
            Source::Current => Self::default(),
            Source::Id(id) => Self {
                fixed: read_named_on_main(&id),
                follows_current: false,
                dead: 0,
            },
        }
    }

    /// Runs `f` on the `UCKeyboardLayout` bytes of the source; `None` if there is none.
    fn with_layout<T>(&self, f: impl FnOnce(*const u8) -> T) -> Option<T> {
        if let Some(fixed) = &self.fixed {
            return Some(f(fixed.as_ptr().cast()));
        }
        if !self.follows_current {
            return None;
        }
        let cur = CURRENT.read().ok()?;
        cur.as_ref().map(|u| f(u.as_ptr().cast()))
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceFlagsState(state_id: i32) -> u64;
}

/// `CGEventFlags` bits: Caps Lock, Shift, Control, Option, Command; and the device-dependent
/// bit for the right Option key.
const F_CAPS: u64 = 0x1_0000;
/// Shift.
const F_SHIFT: u64 = 0x2_0000;
/// Control.
const F_CTRL: u64 = 0x4_0000;
/// Option.
const F_OPTION: u64 = 0x8_0000;
/// Command.
const F_CMD: u64 = 0x10_0000;
/// `NX_DEVICERALTKEYMASK`: the right Option key.
const F_RIGHT_OPTION: u64 = 0x40;

/// [`OsMods`] from `CGEventFlags`.
pub fn mods_from_flags(flags: u64) -> OsMods {
    let option = flags & F_OPTION != 0;
    OsMods {
        shift: flags & F_SHIFT != 0,
        ctrl: flags & F_CTRL != 0,
        alt: option && flags & F_RIGHT_OPTION == 0,
        alt_gr: option && flags & F_RIGHT_OPTION != 0,
        gui: flags & F_CMD != 0,
        caps: flags & F_CAPS != 0,
    }
}

/// The modifier keys and Caps Lock as the OS sees them right now (physical keys that were
/// not swallowed). Safe from any thread.
pub fn os_mods() -> OsMods {
    // SAFETY: a plain query of the session's event-source state; no pointers involved.
    mods_from_flags(unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) })
}

/// The element of `list` (a CFArray of input sources) whose ID is `id`, borrowed; null if none.
///
/// # Safety
///
/// `list` must be a live CFArray of `TISInputSourceRef`s.
unsafe fn find(list: CfRef, id: &str) -> CfRef {
    // SAFETY: indices are within the array's count; each element and its ID string are
    // borrowed and only read here, into a buffer of the length we pass.
    unsafe {
        for i in 0..CFArrayGetCount(list) {
            let src = CFArrayGetValueAtIndex(list, i);
            let name = TISGetInputSourceProperty(src, kTISPropertyInputSourceID);
            let mut buf = [0 as c_char; 128];
            if !name.is_null()
                && CFStringGetCString(name, buf.as_mut_ptr(), 128, UTF8) != 0
                && CStr::from_ptr(buf.as_ptr()).to_bytes() == id.as_bytes()
            {
                return src;
            }
        }
        null()
    }
}

impl Layout for MacLayout {
    const OPTION_TYPES: bool = true;

    fn refresh(&mut self) {
        if self.follows_current {
            refresh_current_layout();
        }
    }

    fn translate(&mut self, code: u32, mods: &Mods) -> Raw {
        let Some(vk) = code_to_vk(code) else { return Raw::None };
        let mut state = 0;
        if mods.shifted() {
            state |= MOD_SHIFT;
        }
        if mods.caps {
            state |= MOD_CAPS;
        }
        if mods.alt() || mods.alt_gr() {
            state |= MOD_OPTION;
        }
        let mut dead = self.dead;
        let translated = self.with_layout(|layout| {
            let mut buf = [0u16; 8];
            let mut len = 0usize;
            // SAFETY: `layout` points at a UCKeyboardLayout that outlives this call;
            // `buf` has the 8 units we declare and `len`/`dead` are valid for writes.
            let status = unsafe {
                UCKeyTranslate(
                    layout,
                    vk,
                    0, // kUCKeyActionDown
                    state,
                    u32::from(LMGetKbdType()),
                    0, // dead keys enabled
                    &mut dead,
                    buf.len(),
                    &mut len,
                    buf.as_mut_ptr(),
                )
            };
            (status == 0).then(|| String::from_utf16_lossy(&buf[..len.min(buf.len())]))
        });
        match translated.flatten() {
            Some(text) => {
                self.dead = dead;
                if text.is_empty() {
                    // No character yet: a dead key if the state moved, else an unmapped key.
                    if dead != 0 { Raw::Dead } else { Raw::None }
                } else {
                    Raw::Text(text)
                }
            }
            None => {
                self.dead = 0;
                Raw::None
            }
        }
    }

    fn reset(&mut self) {
        self.dead = 0;
    }

    fn os_mods(&mut self) -> Option<OsMods> {
        Some(os_mods())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{KeyChar, Typist};

    /// TIS is not safe to call from several threads at once, and the test threads are not the
    /// main queue; tests that read it take this lock (production reads only on the main queue).
    static TIS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Types HID usage `code` once, with Shift/Option held as asked.
    fn type_key(t: &mut Typist<MacLayout>, code: u32, shift: bool, option: bool) -> Option<KeyChar> {
        for (m, on) in [(0xE1, shift), (0xE2, option)] {
            if on {
                t.key(m, true, true);
            }
        }
        let out = t.key(code, true, true);
        t.key(code, false, true);
        for (m, on) in [(0xE1, shift), (0xE2, option)] {
            if on {
                t.key(m, false, true);
            }
        }
        out
    }

    #[test]
    fn os_flags_become_modifiers() {
        assert_eq!(mods_from_flags(0), OsMods::default());
        let m = mods_from_flags(F_CAPS | F_SHIFT | F_CTRL | F_CMD);
        assert!(m.caps && m.shift && m.ctrl && m.gui && !m.alt && !m.alt_gr);
        assert!(mods_from_flags(F_OPTION).alt && !mods_from_flags(F_OPTION).alt_gr);
        assert!(mods_from_flags(F_OPTION | F_RIGHT_OPTION).alt_gr);
        assert_eq!(
            mods_from_flags(F_RIGHT_OPTION),
            OsMods::default(),
            "a device bit alone is nothing"
        );
    }

    #[test]
    fn a_layout_blob_shorter_than_its_header_is_refused() {
        assert!(aligned_copy(&[]).is_none());
        assert!(aligned_copy(&[0; MIN_UCHR_LEN - 1]).is_none());
        let copy = aligned_copy(&[7; MIN_UCHR_LEN + 3]).expect("long enough");
        assert_eq!(copy.len(), (MIN_UCHR_LEN + 3).div_ceil(8));
        assert_eq!(copy[0], u64::from_ne_bytes([7; 8]));
    }

    #[test]
    fn a_named_layout_from_a_non_main_thread_never_hangs() {
        // The test harness's main thread is not servicing the main queue: no layout, in bounded time.
        let started = std::time::Instant::now();
        let l = MacLayout::new(Source::Id("com.apple.keylayout.German".into()));
        assert!(started.elapsed() < MAIN_QUEUE_WAIT + std::time::Duration::from_secs(5));
        assert!(l.fixed.is_none());
    }

    #[test]
    fn no_cached_layout_means_no_character() {
        // A fixed lookup over a source that does not exist answers None, never garbage.
        let _tis = TIS.lock().unwrap();
        let mut t = Typist::new(MacLayout::new(Source::Id("no.such.layout".into())));
        assert_eq!(type_key(&mut t, 0x04, false, false), None);
    }

    fn text(s: &str) -> Option<KeyChar> {
        Some(KeyChar::Text(s.into()))
    }

    #[test]
    fn the_current_layout_translates_letters() {
        // Whatever layout the machine has, the A key (usage 0x04) is a letter and Shift changes its case.
        // The test thread is not the main queue; reading TIS here is tolerated in tests only.
        let _tis = TIS.lock().unwrap();
        let fresh = read_current();
        *CURRENT.write().unwrap() = fresh;
        let mut t = Typist::new(MacLayout::default());
        let a = type_key(&mut t, 0x04, false, false);
        let big = type_key(&mut t, 0x04, true, false);
        assert!(matches!(a, Some(KeyChar::Text(_))), "{a:?}");
        assert_ne!(a, big);
    }

    #[test]
    fn a_named_german_layout_types_german() {
        // Skipped where German is not installed (it ships with macOS); never changes the active layout.
        let german = Source::Id("com.apple.keylayout.German".into());
        let _tis = TIS.lock().unwrap();
        let Source::Id(id) = german else { unreachable!() };
        // The test thread is not the main thread; reading TIS here is tolerated in tests only.
        let Some(fixed) = read_named(&id) else { return };
        let mut t = Typist::new(MacLayout {
            fixed: Some(fixed),
            follows_current: false,
            dead: 0,
        });
        assert_eq!(type_key(&mut t, 0x1C, false, false), text("z"), "Y position is z");
        assert_eq!(type_key(&mut t, 0x1D, false, false), text("y"), "Z position is y");
        assert_eq!(type_key(&mut t, 0x33, false, false), text("ö"), "semicolon position");
        assert_eq!(type_key(&mut t, 0x2D, false, false), text("ß"));
        assert_eq!(type_key(&mut t, 0x0F, false, true), text("@"), "Option+L is @");
        assert_eq!(
            type_key(&mut t, 0x08, false, true),
            text("€"),
            "Option+E is the euro sign"
        );
        // Dead acute (the key left of Backspace) then e composes to é.
        assert_eq!(type_key(&mut t, 0x2E, false, false), Some(KeyChar::Pending));
        assert_eq!(type_key(&mut t, 0x08, false, false), text("é"));
        // A dead key then a letter that does not compose gives the accent and the letter.
        assert_eq!(type_key(&mut t, 0x2E, false, false), Some(KeyChar::Pending));
        assert_eq!(type_key(&mut t, 0x0E, false, false), text("´k"));
    }
}
