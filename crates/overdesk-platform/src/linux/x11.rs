// SPDX-License-Identifier: AGPL-3.0-only
//! X11 capture (XInput2 raw events) and injection (XTest). Same API as the macOS and
//! Windows adapters. Pure Rust (`x11rb`, no unsafe), per user, no evdev or uinput.
//!
//! Xorg and XWayland only; `ffi` picks this backend for X11 sessions and the portal backend
//! for Wayland ones.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use overdesk_proto::{Button, Phase, ScrollUnit};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _, Fp3232, XIEventMask};
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, GrabStatus,
    WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

use super::events::{Notches, button_from_x, wheel_from_x, x_button};
use super::ffi::LinuxError;
use super::keymap::{code_to_keycode, keycode_to_code};
use crate::input::Captured;
use crate::input::EscapeChord;
use crate::mac::events::Subpixel;

/// Most wheel clicks one scroll event may inject, so a runaway remote delta cannot flood
/// the server.
const MAX_CLICKS: i32 = 64;
/// `CLIENT_MESSAGE` data word telling the capture thread to start grabbing local input.
const CMD_GRAB: u32 = 1;
/// `CLIENT_MESSAGE` data word telling the capture thread to release the grab.
const CMD_UNGRAB: u32 = 2;
/// `CLIENT_MESSAGE` data word telling the capture thread to exit.
const CMD_QUIT: u32 = 3;

/// Wraps any X11 error as [`LinuxError::X11`].
fn x11(e: impl std::fmt::Display) -> LinuxError {
    LinuxError::X11(e.to_string())
}

/// Connects to the X server of this session and returns the connection with the root
/// window of its default screen. Refuses a missing `DISPLAY`.
fn connect() -> Result<(RustConnection, xproto::Window), LinuxError> {
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    if var("DISPLAY").is_empty() {
        return Err(LinuxError::NoDisplay);
    }
    let (conn, screen) = RustConnection::connect(None).map_err(x11)?;
    let root = conn.setup().roots[screen].root;
    Ok((conn, root))
}

/// Desktop size in pixels: the root window, which spans every monitor (RandR places them
/// inside it), so this is the bounding box and root coordinates are already box-relative
/// with the origin at 0,0. `(0, 0)` when no X server is reachable.
///
/// shortcut: monitors that leave holes in the root (an L-shaped layout) are unreachable
/// there; see [`crate::input::bounding_box`].
pub fn main_display_size() -> (u32, u32) {
    let Ok((conn, root)) = connect() else { return (0, 0) };
    conn.get_geometry(root)
        .ok()
        .and_then(|c| c.reply().ok())
        .map_or((0, 0), |g| (g.width.into(), g.height.into()))
}

/// A running capture. Dropping it stops the capture thread and releases any grab.
pub struct Capture {
    /// The capture thread's connection, shared so [`Capture::set_swallow`] can send it commands.
    conn: Arc<RustConnection>,
    /// Hidden window owned by the capture thread; commands are client messages sent to it,
    /// which wakes the thread out of its blocking wait.
    wake: xproto::Window,
    /// The capture thread; `None` only after it has been joined.
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Starts capture on its own thread and returns the receiver of captured input. Input
    /// reaches local apps unmodified until [`Capture::set_swallow`] is turned on. Waits until
    /// the raw-event selection is in place, so an error is reported here, not lost.
    ///
    /// Input injected through XTest (by [`Injector`] or `xdotool`) is ignored, so a peer
    /// that both injects and captures never loops; set `OVERDESK_X11_CAPTURE_XTEST=1` to
    /// capture it anyway (for tests).
    pub fn start() -> Result<(Self, Receiver<Captured>), LinuxError> {
        let (conn, root) = connect()?;
        let conn = Arc::new(conn);
        conn.xinput_xi_query_version(2, 0).map_err(x11)?.reply().map_err(x11)?;
        let ignore = if capture_xtest(std::env::var_os("OVERDESK_X11_CAPTURE_XTEST").as_deref()) {
            HashSet::new()
        } else {
            xtest_devices(&conn)?
        };
        let wake = conn.generate_id().map_err(x11)?;
        conn.create_window(
            0,
            wake,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .map_err(x11)?;
        let mask = XIEventMask::RAW_MOTION
            | XIEventMask::RAW_BUTTON_PRESS
            | XIEventMask::RAW_BUTTON_RELEASE
            | XIEventMask::RAW_KEY_PRESS
            | XIEventMask::RAW_KEY_RELEASE;
        // Raw events go to every client that selects them, grab or not, so the escape
        // chord keeps working while input is swallowed.
        let select = [xinput::EventMask {
            deviceid: xinput::Device::ALL_MASTER.into(),
            mask: vec![mask],
        }];
        conn.xinput_xi_select_events(root, &select)
            .map_err(x11)?
            .check()
            .map_err(x11)?;
        // Best effort: without XFixes the cursor merely stays visible while swallowing.
        let _ = conn.xfixes_query_version(4, 0).map(|c| c.reply());
        let (tx, rx) = channel();
        let mut state = Thread {
            conn: conn.clone(),
            root,
            tx,
            ignore,
            chord: EscapeChord::default(),
            motion: Subpixel::default(),
            frozen: None,
            warned: false,
        };
        let thread = std::thread::Builder::new()
            .name("overdesk-x11-capture".into())
            .spawn(move || state.run())
            .map_err(x11)?;
        Ok((
            Self {
                conn,
                wake,
                thread: Some(thread),
            },
            rx,
        ))
    }

    /// While on, local apps stop receiving input (active pointer and keyboard grabs, cursor
    /// hidden and held in place). Asynchronous: the capture thread applies it in order. If
    /// another client already holds a grab it sends [`Captured::Escape`] instead, so the
    /// engine returns to local control rather than driving a peer while input still reaches
    /// local apps.
    pub fn set_swallow(&self, on: bool) {
        self.command(if on { CMD_GRAB } else { CMD_UNGRAB });
    }

    /// Sends `cmd` to the capture thread as a client message to its hidden window.
    fn command(&self, cmd: u32) {
        let msg = ClientMessageEvent::new(32, self.wake, AtomEnum::CARDINAL, [cmd, 0, 0, 0, 0]);
        // A failed send means the connection is gone, and with it the grabs.
        let _ = self.conn.send_event(false, self.wake, EventMask::NO_EVENT, msg);
        let _ = self.conn.flush();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.command(CMD_QUIT);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Device ids of the XTest virtual pointer and keyboard: the source of every injected event.
fn xtest_devices(conn: &RustConnection) -> Result<HashSet<u16>, LinuxError> {
    let devices = conn
        .xinput_xi_query_device(xinput::Device::ALL)
        .map_err(x11)?
        .reply()
        .map_err(x11)?;
    Ok(devices
        .infos
        .iter()
        .filter(|d| d.name.starts_with(b"Virtual core XTEST"))
        .map(|d| d.deviceid)
        .collect())
}

/// Reads `axis` (valuator number) out of a raw event's packed valuators: `mask` says which
/// axes are present and `values` holds those only, in axis order.
fn valuator(mask: &[u32], values: &[Fp3232], axis: usize) -> f64 {
    let present = |i: usize| mask.get(i / 32).is_some_and(|w| w >> (i % 32) & 1 == 1);
    if !present(axis) {
        return 0.0;
    }
    let index = (0..axis).filter(|&i| present(i)).count();
    values
        .get(index)
        .map_or(0.0, |v| f64::from(v.integral) + f64::from(v.frac) / 4_294_967_296.0)
}

/// Whether `OVERDESK_X11_CAPTURE_XTEST` asks to capture XTest input: only the value `1`
/// counts, so `=0` (the natural way to switch it off) does not enable it.
fn capture_xtest(v: Option<&std::ffi::OsStr>) -> bool {
    v == Some(std::ffi::OsStr::new("1"))
}

/// The capture thread's state; it owns every grab so releasing them never crosses threads.
struct Thread {
    /// Shared with [`Capture`], which only sends it client messages.
    conn: Arc<RustConnection>,
    /// Root window of the default screen.
    root: xproto::Window,
    /// Where captured input goes; the receiving end belongs to the engine.
    tx: Sender<Captured>,
    /// Source device ids whose events are ours (XTest) and must not be captured again.
    ignore: HashSet<u16>,
    /// Failsafe detector for the escape chord.
    chord: EscapeChord,
    /// Carries the fractional part of accelerated motion between events.
    motion: Subpixel,
    /// While swallowing: where the cursor is held. `None` while input is local.
    frozen: Option<(i16, i16)>,
    /// Whether a refused grab was already reported on stderr.
    warned: bool,
}

impl Thread {
    /// Handles events until told to quit or the connection dies. Dropping `self` closes the
    /// last strong reference held here; the grabs end with the connection at the latest.
    fn run(&mut self) {
        while let Ok(event) = self.conn.wait_for_event() {
            let proceed = match event {
                Event::ClientMessage(m) => self.command(m.data.as_data32()[0]),
                Event::XinputRawMotion(e) if !self.ignore.contains(&e.sourceid) => {
                    let dx = valuator(&e.valuator_mask, &e.axisvalues, 0);
                    let dy = valuator(&e.valuator_mask, &e.axisvalues, 1);
                    self.moved(dx, dy)
                }
                Event::XinputRawButtonPress(e) if !self.ignore.contains(&e.sourceid) => self.button(e.detail, true),
                Event::XinputRawButtonRelease(e) if !self.ignore.contains(&e.sourceid) => self.button(e.detail, false),
                Event::XinputRawKeyPress(e) => self.key_from(e.sourceid, e.detail, true),
                Event::XinputRawKeyRelease(e) => self.key_from(e.sourceid, e.detail, false),
                _ => true,
            };
            // The engine is gone: stop swallowing, or local input would be lost for good.
            if !proceed {
                break;
            }
        }
        self.release();
    }

    /// Routes a key edge by source. Keys from ignored (XTest) devices are normally dropped,
    /// but while swallowing they still reach our grab (VNC and xdotool-style tools), so they
    /// feed the escape chord alone: otherwise the failsafe would be dead for those keys.
    fn key_from(&mut self, source: u16, keycode: u32, pressed: bool) -> bool {
        if !self.ignore.contains(&source) {
            self.key(keycode, pressed, true)
        } else if self.frozen.is_some() {
            self.key(keycode, pressed, false)
        } else {
            true
        }
    }

    /// Sends `c` to the engine; `false` when nobody listens any more.
    fn send(&self, c: Captured) -> bool {
        self.tx.send(c).is_ok()
    }

    /// Runs one command from [`Capture::set_swallow`] or drop; `false` ends the thread.
    fn command(&mut self, cmd: u32) -> bool {
        match cmd {
            CMD_GRAB => self.grab(),
            CMD_UNGRAB => {
                self.release();
                true
            }
            CMD_QUIT => false,
            _ => true,
        }
    }

    /// Grabs pointer and keyboard on the root window so local apps stop receiving input,
    /// hides the cursor and remembers where to hold it. If either grab is refused, undoes
    /// the other and reports [`Captured::Escape`]: the engine treats that as "return to
    /// local", the only signal in the API that stops it believing it is driving a peer.
    fn grab(&mut self) -> bool {
        if self.frozen.is_some() {
            return true;
        }
        let ok = |status: Result<GrabStatus, ()>| status == Ok(GrabStatus::SUCCESS);
        let pointer = ok(self
            .conn
            .grab_pointer(
                false,
                self.root,
                EventMask::NO_EVENT,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                NONE,
                CURRENT_TIME,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.status)
            .ok_or(()));
        let keyboard = pointer
            && ok(self
                .conn
                .grab_keyboard(false, self.root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.status)
                .ok_or(()));
        if !(pointer && keyboard) {
            if pointer {
                let _ = self.conn.ungrab_pointer(CURRENT_TIME);
                let _ = self.conn.flush();
            }
            if !self.warned {
                self.warned = true;
                eprintln!("X11: another client holds the pointer or keyboard grab; staying on local input");
            }
            return self.send(Captured::Escape);
        }
        let _ = self.conn.xfixes_hide_cursor(self.root);
        let at = self.pointer();
        self.frozen = Some(at);
        let _ = self.conn.flush();
        true
    }

    /// Ends the grabs and shows the cursor again. Safe to call when nothing is grabbed.
    fn release(&mut self) {
        if self.frozen.take().is_some() {
            let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
            let _ = self.conn.xfixes_show_cursor(self.root);
            let _ = self.conn.flush();
        }
    }

    /// Where the cursor is now, root-relative.
    fn pointer(&self) -> (i16, i16) {
        self.conn
            .query_pointer(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or((0, 0), |p| (p.root_x, p.root_y))
    }

    /// Reports one raw motion. While swallowing the cursor is warped back to its held spot
    /// (a core warp makes no raw event), so it never reaches a screen edge and stalls; the
    /// raw deltas keep flowing regardless.
    fn moved(&mut self, dx: f64, dy: f64) -> bool {
        let (dx, dy) = self.motion.feed(dx, dy);
        if dx == 0 && dy == 0 {
            return true;
        }
        let (x, y) = match self.frozen {
            Some((x, y)) => {
                let _ = self.conn.warp_pointer(NONE, self.root, 0, 0, 0, 0, x, y);
                let _ = self.conn.flush();
                (x, y)
            }
            None => self.pointer(),
        };
        self.send(Captured::Moved {
            dx,
            dy,
            x: x.into(),
            y: y.into(),
        })
    }

    /// Reports a raw button edge: buttons 1-3, 8, 9 as buttons, 4-7 presses as scroll clicks.
    fn button(&mut self, n: u32, pressed: bool) -> bool {
        if let Some(button) = button_from_x(n) {
            return self.send(Captured::Button { button, pressed });
        }
        match wheel_from_x(n) {
            Some((dx, dy)) if pressed => self.send(Captured::Scroll {
                dx,
                dy,
                unit: ScrollUnit::Notch120,
                phase: Phase::None,
            }),
            _ => true,
        }
    }

    /// Reports a raw key edge, checking the escape chord first so it also fires while
    /// grabbed: it releases the grabs itself, then tells the engine.
    /// `forward` false feeds the chord only and sends no `Key` event.
    fn key(&mut self, keycode: u32, pressed: bool, forward: bool) -> bool {
        let Some(code) = keycode_to_code(keycode) else {
            return true;
        };
        if self.chord.key(code, pressed) {
            self.release();
            if !self.send(Captured::Escape) {
                return false;
            }
        }
        !forward || self.send(Captured::Key { code, pressed })
    }
}

/// A panic on the capture thread unwinds through here: without it the grabs and the hidden
/// cursor would outlive the thread (the connection stays open via `Capture`), leaving local
/// input dead and the escape chord gone. `release` is idempotent and only ignores errors.
impl Drop for Thread {
    fn drop(&mut self) {
        self.release();
    }
}

/// Injects input through XTest; apps cannot tell it from a real keyboard and mouse.
pub struct Injector {
    /// Connection used for every `FakeInput`.
    conn: RustConnection,
    /// Root window of the default screen.
    root: xproto::Window,
    /// Overdesk codes this injector has pressed and not yet released.
    keys: Vec<u32>,
    /// Buttons this injector has pressed and not yet released.
    buttons: Vec<Button>,
    /// Turns Mac trackpad pixels into wheel clicks.
    scroll: Notches,
}

/// If the injector ends mid-session, keys and buttons it pressed through XTest would stay
/// held on the X server (stuck modifiers); release them on every exit path, panics included.
impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl Injector {
    /// Creates an injector for the whole root window (all monitors); fails if the XTest extension is missing.
    pub fn new() -> Result<Self, LinuxError> {
        let (conn, root) = connect()?;
        conn.xtest_get_version(2, 2).map_err(x11)?.reply().map_err(x11)?;
        Ok(Self {
            conn,
            root,
            keys: Vec::new(),
            buttons: Vec::new(),
            scroll: Notches::default(),
        })
    }

    /// Where the system cursor is now (pixels); `(0, 0)` if no X server is reachable.
    pub fn cursor() -> (f64, f64) {
        /// One long-lived connection for polling, so a caller that asks per event
        /// does not reconnect each time.
        static QUERY: OnceLock<Option<(RustConnection, xproto::Window)>> = OnceLock::new();
        let Some((conn, root)) = QUERY.get_or_init(|| connect().ok()) else {
            return (0.0, 0.0);
        };
        conn.query_pointer(*root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or((0.0, 0.0), |p| (p.root_x.into(), p.root_y.into()))
    }

    /// Sends one `FakeInput` event and flushes, so it takes effect before the next call.
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        // A failed send means the server is gone; there is nothing useful to retry.
        let _ = self.conn.xtest_fake_input(kind, detail, 0, self.root, x, y, 0);
        let _ = self.conn.flush();
    }

    /// Moves the pointer to root-window pixel coordinates.
    pub fn pointer(&mut self, x: f64, y: f64) {
        let clamp = |v: f64| v.round().clamp(0.0, f64::from(i16::MAX)) as i16;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, clamp(x), clamp(y));
    }

    /// Presses or releases `button` at the current cursor position, tracking it so
    /// [`Injector::release_all`] can release it later.
    pub fn button(&mut self, button: Button, pressed: bool) {
        self.buttons.retain(|b| *b != button);
        if pressed {
            self.buttons.push(button);
        }
        self.click(x_button(button), pressed);
    }

    /// One press or release of X11 button `n`.
    fn click(&self, n: u8, pressed: bool) {
        let kind = if pressed {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.fake(kind, n, 0, 0);
    }

    /// Returns `false` for codes Linux has no key for (e.g. Fn/Globe).
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        let Some(keycode) = code_to_keycode(code) else {
            return false;
        };
        self.keys.retain(|k| *k != code);
        if pressed {
            self.keys.push(code);
        }
        let kind = if pressed {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.fake(kind, keycode, 0, 0);
        true
    }

    /// Scrolls by `dx`, `dy` in `unit`s as wheel clicks (buttons 4/5 vertical, 6/7
    /// horizontal); trackpad pixels accumulate until a whole click is due. X11 has no
    /// scroll phases, so `_phase` is ignored.
    pub fn scroll(&mut self, dx: i32, dy: i32, unit: ScrollUnit, _phase: Phase) {
        let (cx, cy) = self.scroll.feed(dx, dy, unit);
        for (clicks, up, down) in [(cy, 4, 5), (cx, 7, 6)] {
            let n = if clicks > 0 { up } else { down };
            for _ in 0..clicks.abs().min(MAX_CLICKS) {
                self.click(n, true);
                self.click(n, false);
            }
        }
    }

    /// Releases every key and button this injector holds down.
    pub fn release_all(&mut self) {
        for code in self.keys.clone().into_iter().rev() {
            self.key(code, false);
        }
        for button in self.buttons.clone() {
            self.button(button, false);
        }
    }
}

/// How long to wait for the selection owner to answer `TARGETS`. This runs every 500 ms
/// during a session, so it stays well under that.
const TARGETS_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(150);

/// True when `targets` (the atoms the owner offers) contains the password-manager `hint`.
fn offers_hint(targets: &[u32], hint: u32) -> bool {
    hint != 0 && targets.contains(&hint)
}

/// Asks the CLIPBOARD owner for its `TARGETS` and reports whether it offers
/// `x-kde-passwordManagerHint`; `None` when the owner did not answer in time or refused.
fn query_private(conn: &RustConnection, root: xproto::Window) -> Result<Option<bool>, LinuxError> {
    let atom = |name: &[u8], only_if_exists| -> Result<u32, LinuxError> {
        Ok(conn
            .intern_atom(only_if_exists, name)
            .map_err(x11)?
            .reply()
            .map_err(x11)?
            .atom)
    };
    let clipboard = atom(b"CLIPBOARD", false)?;
    let targets = atom(b"TARGETS", false)?;
    // Absent atom (0) means nobody on this server ever used the hint, so nothing is private.
    let hint = atom(b"x-kde-passwordManagerHint", true)?;
    if conn
        .get_selection_owner(clipboard)
        .map_err(x11)?
        .reply()
        .map_err(x11)?
        .owner
        == NONE
    {
        return Ok(Some(false));
    }
    let win = conn.generate_id().map_err(x11)?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(x11)?;
    let result = (|| {
        conn.convert_selection(win, clipboard, targets, targets, CURRENT_TIME)
            .map_err(x11)?;
        conn.flush().map_err(x11)?;
        let deadline = std::time::Instant::now() + TARGETS_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if let Some(Event::SelectionNotify(n)) = conn.poll_for_event().map_err(x11)? {
                if n.requestor != win || n.property == NONE {
                    return Ok(None); // refused: cannot tell
                }
                let reply = conn
                    .get_property(true, win, n.property, AtomEnum::ATOM, 0, 4096)
                    .map_err(x11)?
                    .reply()
                    .map_err(x11)?;
                // Presence of the target is the convention (Klipper, KeePassXC, Bitwarden set it
                // with the value "secret"); a copy that offers the target at all is not shared.
                return Ok(Some(
                    reply.value32().is_some_and(|mut v| v.any(|a| offers_hint(&[a], hint))),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(None)
    })();
    let _ = conn.destroy_window(win);
    let _ = conn.flush();
    result
}

/// True when the clipboard is marked private by the app that set it (it offers the
/// `x-kde-passwordManagerHint` target), so the copy must not leave this computer.
///
/// Failure policy: when the owner exists but does not answer in time (or refuses `TARGETS`)
/// the copy cannot be proven harmless, so this returns `true` and the copy is skipped; the
/// next 500 ms tick tries again. When the X server itself is unreachable it returns `false`:
/// the input adapter is dead then too, and failing closed would silently disable sharing
/// forever.
pub fn clipboard_is_private() -> bool {
    match connect() {
        Ok((conn, root)) => match query_private(&conn, root) {
            Ok(Some(private)) => private,
            Ok(None) => true,
            Err(_) => false,
        },
        Err(_) => false, // no DISPLAY, or X unreachable
    }
}

#[cfg(test)]
mod private_tests {
    use super::{capture_xtest, offers_hint};
    use std::ffi::OsStr;

    #[test]
    fn xtest_override_needs_exactly_one() {
        assert!(capture_xtest(Some(OsStr::new("1"))));
        assert!(!capture_xtest(Some(OsStr::new("0"))));
        assert!(!capture_xtest(Some(OsStr::new(""))));
        assert!(!capture_xtest(None));
    }

    #[test]
    fn hint_is_found_only_when_interned_and_offered() {
        assert!(offers_hint(&[10, 42, 7], 42));
        assert!(!offers_hint(&[10, 7], 42));
        assert!(!offers_hint(&[0, 7], 0)); // atom 0 = never interned
    }
}
