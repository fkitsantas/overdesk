// SPDX-License-Identifier: AGPL-3.0-only
//! Wayland capture and injection through the desktop portals (design: `docs/WAYLAND.md`).
//!
//! Capture uses `InputCapture`: barriers on the outer edges of the union of all screens, and while the
//! compositor has capture active, input arrives over EIS (`reis`, receiver side). Injection
//! uses `RemoteDesktop` with the `Notify*` calls. One tokio thread owns every portal session;
//! the adapters talk to it over channels. All decisions live in `wayland_core`.
//!
//! shortcut: injection uses the portal's D-Bus `Notify*` methods, not `ConnectToEIS`: same
//! consent and codes, no sender-side EIS device handshake. Move to EIS if per-event D-Bus
//! latency shows up on hardware.

use std::future::Future;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ashpd::AppID;
use ashpd::desktop::PersistMode;
use ashpd::desktop::input_capture::{
    ActivatedBarrier, Barrier, Capabilities, CreateSessionOptions, InputCapture, ReleaseOptions,
};
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use futures_util::StreamExt;
use overdesk_proto::{Button, Gesture, Phase, ScrollUnit};
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::ffi::LinuxError;
use super::keymap::code_to_keycode;
use super::wayland_core::{
    Input, MAX_STEPS, Machine, Out, Pointer, ScrollOut, Zone, desk_bounds, evdev_from_button, home_corner,
    plan_barriers, portal_unavailable, scroll_out,
};
use crate::input::{
    CTRL_KEY, Captured, Notches, Pressed, Rect, Sink, Warning, Warnings, box_relative, chord_events, pinch_wheel,
};

/// The id Overdesk registers with the portal as an unsandboxed app (basename of its
/// `.desktop` file); see `docs/WAYLAND.md`.
const APP_ID: &str = "org.overdesk.Overdesk";
/// File in the store directory that keeps the RemoteDesktop restore token.
const TOKEN_FILE: &str = "wayland-restore-token";
/// How long dropping an adapter waits for its portal task to finish releasing.
const DROP_WAIT: Duration = Duration::from_secs(1);

/// Desk box (all zones) size as read from the portal zones; `None` before capture armed.
static PRIMARY: Mutex<Option<(u32, u32)>> = Mutex::new(None);
/// The portal zones relative to the desk box, as read when capture armed.
static MONITORS: Mutex<Vec<Rect>> = Mutex::new(Vec::new());
/// Where the pointer is, shared so a capture release tells the injector where it landed.
static POINTER: Mutex<Option<Pointer>> = Mutex::new(None);
/// Job queue of the portal thread, created on first use.
static RUNTIME: Mutex<Option<UnboundedSender<Job>>> = Mutex::new(None);

/// Runs `f` on the shared pointer tracker.
fn with_pointer<R>(f: impl FnOnce(&mut Pointer) -> R) -> R {
    let mut g = POINTER.lock().unwrap_or_else(PoisonError::into_inner);
    f(g.get_or_insert_with(Pointer::default))
}

/// Turns any error into the Wayland startup error, with the X11 fallback explanation.
fn unavailable(e: impl std::fmt::Display) -> LinuxError {
    LinuxError::Wayland(portal_unavailable(
        &e.to_string(),
        std::env::var("DISPLAY").ok().as_deref(),
    ))
}

/// A task to start on the portal thread.
type Job = Box<dyn FnOnce() + Send>;

/// Starts `make()`'s future on the single thread that runs every portal session, creating
/// the thread on first use. One thread for the process: the shared D-Bus connection must
/// outlive any one adapter, and the EIS streams are not `Send`, so tasks are local to it.
fn spawn_portal<F, Fut>(make: F) -> Result<(), LinuxError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let mut g = RUNTIME.lock().unwrap_or_else(PoisonError::into_inner);
    if g.is_none() {
        let (jobs, mut rx) = unbounded_channel::<Job>();
        let (ok_tx, ok) = channel();
        std::thread::Builder::new()
            .name("overdesk-portal".into())
            .spawn(
                move || match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => {
                        let _ = ok_tx.send(Ok(()));
                        let local = tokio::task::LocalSet::new();
                        rt.block_on(local.run_until(async move {
                            while let Some(job) = rx.recv().await {
                                job();
                            }
                        }));
                    }
                    Err(e) => {
                        let _ = ok_tx.send(Err(e.to_string()));
                    }
                },
            )
            .map_err(unavailable)?;
        ok.recv().map_err(unavailable)?.map_err(unavailable)?;
        *g = Some(jobs);
    }
    let job: Job = Box::new(move || {
        tokio::task::spawn_local(make());
    });
    g.as_ref()
        .ok_or_else(|| unavailable("portal thread missing"))?
        .send(job)
        .map_err(|_| unavailable("portal thread stopped"))
}

/// What a portal task reports through: `Ok` once it is armed, `Err` if it failed before that.
type Ready = Option<Sender<Result<(), String>>>;

/// Runs `task` on the portal thread, waits until it reports ready (or fails first), and
/// returns the receiver that fires when the task has finished. A failure after that is
/// reported to `warnings` as `what` ended.
fn start_portal_task<F>(what: &'static str, warnings: Warnings, task: F) -> Result<Receiver<()>, LinuxError>
where
    F: for<'a> FnOnce(&'a mut Ready) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>
        + Send
        + 'static,
{
    let (ready_tx, ready) = channel();
    let (done_tx, done) = channel();
    spawn_portal(move || async move {
        let mut ready_tx = Some(ready_tx);
        if let Err(e) = task(&mut ready_tx).await {
            match ready_tx.take() {
                Some(r) => {
                    let _ = r.send(Err(e));
                }
                None => warnings.raise(Warning::PortalEnded { what, error: e }),
            }
        }
        let _ = done_tx.send(());
    })?;
    ready
        .recv()
        .map_err(|_| unavailable("portal task stopped"))?
        .map_err(unavailable)?;
    Ok(done)
}

/// Desktop size in pixels (bounding box of all monitors); `(0, 0)` until capture has read the zones.
pub fn main_display_size() -> (u32, u32) {
    PRIMARY.lock().unwrap_or_else(PoisonError::into_inner).unwrap_or((0, 0))
}

/// The portal zones, relative to the desk box; empty until capture has read them.
pub fn monitors() -> Vec<Rect> {
    MONITORS.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Registers this unsandboxed process with the portal under [`APP_ID`], so the portal can
/// key permissions and restore tokens to Overdesk. At most once per connection and only for
/// unsandboxed apps: a repeat or a refusal (older portals) is harmless and ignored.
async fn register() {
    if let Ok(id) = AppID::try_from(APP_ID) {
        let _ = ashpd::register_host_app(id).await;
    }
}

/// Path of the stored RemoteDesktop restore token in the store folder `dir`.
fn token_path(dir: &Path) -> PathBuf {
    dir.join(TOKEN_FILE)
}

/// The saved restore token, if any.
fn load_token(dir: &Path) -> Option<String> {
    let t = std::fs::read_to_string(token_path(dir)).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// Saves the restore token with owner-only permissions. Tokens are single use, so this runs
/// after every start.
fn save_token(dir: &Path, token: &str) -> std::io::Result<()> {
    let path = token_path(dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(token.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

/// Commands to the capture task.
enum Cmd {
    /// The engine turned swallowing on or off.
    Swallow(bool),
}

/// Tells the engine to return to local input when the capture task ends for any reason
/// (error, disconnect, panic, shutdown), so the user is never left without local input.
struct EscapeOnDrop(Sink);

impl Drop for EscapeOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(Captured::Escape);
    }
}

/// A running capture. Dropping it ends the portal session, which releases any capture.
pub struct Capture {
    /// Commands to the capture task; closing it ends the task.
    cmds: UnboundedSender<Cmd>,
    /// Signalled when the task has closed its session.
    done: Mutex<Receiver<()>>,
}

impl Capture {
    /// Creates the InputCapture session, sets barriers and enables capture, delivering every
    /// event to `tx` on the portal thread. Blocks until it is armed (the compositor may ask
    /// for consent first). Errors say the portal is unavailable and why X11 is not used instead.
    pub fn start_with(tx: Sink) -> Result<Self, LinuxError> {
        let (cmds, cmd_rx) = unbounded_channel();
        let warnings = tx.warnings().clone();
        let done = start_portal_task("capture", warnings, move |ready| {
            Box::pin(async move {
                let _guard = EscapeOnDrop(tx.clone());
                capture(&tx, cmd_rx, ready).await
            })
        })?;
        Ok(Self {
            cmds,
            done: Mutex::new(done),
        })
    }

    /// While on, the compositor keeps input from local apps (the activation is claimed); off
    /// releases it. Asynchronous.
    pub fn set_swallow(&self, on: bool) {
        let _ = self.cmds.send(Cmd::Swallow(on));
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Closing the channel ends the task, which closes the session.
        let (dead, _) = unbounded_channel();
        drop(std::mem::replace(&mut self.cmds, dead));
        let _ = self
            .done
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(DROP_WAIT);
    }
}

/// Reads the portal zones, sets barriers on the union's outer edges and enables capture.
async fn arm(ic: &InputCapture, session: &ashpd::desktop::Session<InputCapture>) -> Result<Vec<Zone>, String> {
    let zones = ic
        .zones(session, Default::default())
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;
    let list: Vec<Zone> = zones
        .regions()
        .iter()
        .map(|r| Zone {
            x: r.x_offset(),
            y: r.y_offset(),
            w: r.width(),
            h: r.height(),
        })
        .collect();
    let primary = desk_bounds(&list).ok_or("the compositor reported no screens")?;
    let barriers: Vec<Barrier> = plan_barriers(primary, &list)
        .into_iter()
        .filter_map(|b| {
            let id = std::num::NonZeroU32::new(b.id)?;
            Some(Barrier::new(id, (b.x1, b.y1, b.x2, b.y2)))
        })
        .collect();
    let done = ic
        .set_pointer_barriers(session, &barriers, zones.zone_set(), Default::default())
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;
    if done.failed_barriers().len() >= barriers.len() {
        return Err("the compositor denied every pointer barrier".into());
    }
    ic.enable(session, Default::default())
        .await
        .map_err(|e| e.to_string())?;
    *PRIMARY.lock().unwrap_or_else(PoisonError::into_inner) = Some((primary.w, primary.h));
    let rects: Vec<Rect> = list.iter().map(Rect::from).collect();
    *MONITORS.lock().unwrap_or_else(PoisonError::into_inner) = box_relative(&rects);
    with_pointer(|p| p.set_home(home_corner(&list)));
    Ok(list)
}

/// Maps one EIS event to a machine input, binding the seat's capabilities when it appears.
fn convert(event: EiEvent, context: &ei::Context) -> Option<Input> {
    Some(match event {
        EiEvent::SeatAdded(s) => {
            s.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::Keyboard
                    | DeviceCapability::Scroll
                    | DeviceCapability::Button,
            );
            let _ = context.flush();
            return None;
        }
        EiEvent::PointerMotion(m) => Input::Motion(f64::from(m.dx), f64::from(m.dy)),
        EiEvent::Button(b) => Input::Button(b.button, b.state == ei::button::ButtonState::Press),
        EiEvent::KeyboardKey(k) => Input::Key(k.key, k.state == ei::keyboard::KeyState::Press),
        EiEvent::ScrollDelta(s) => Input::ScrollSmooth(f64::from(s.dx), f64::from(s.dy)),
        EiEvent::ScrollDiscrete(s) => Input::ScrollDiscrete(s.discrete_dx, s.discrete_dy),
        EiEvent::Disconnected(_) => Input::Failed,
        _ => return None,
    })
}

/// The capture task: owns the session and the EIS stream and drives the state machine.
/// Returns when the adapter is dropped, the engine is gone or the session fails.
async fn capture(tx: &Sink, mut cmds: UnboundedReceiver<Cmd>, ready: &mut Ready) -> Result<(), String> {
    register().await;
    let ic = InputCapture::new().await.map_err(|e| e.to_string())?;
    let (session, _caps) = ic
        .create_session(
            None,
            CreateSessionOptions::default().set_capabilities(Capabilities::KEYBOARD | Capabilities::POINTER),
        )
        .await
        .map_err(|e| e.to_string())?;
    let result = capture_session(tx, &mut cmds, ready, &ic, &session).await;
    let _ = session.close().await;
    result
}

/// The body of [`capture`], split out so the session is always closed afterwards.
async fn capture_session(
    tx: &Sink,
    cmds: &mut UnboundedReceiver<Cmd>,
    ready: &mut Ready,
    ic: &InputCapture,
    session: &ashpd::desktop::Session<InputCapture>,
) -> Result<(), String> {
    let fd = ic
        .connect_to_eis(session, Default::default())
        .await
        .map_err(|e| e.to_string())?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    let context = ei::Context::new(stream).map_err(|e| e.to_string())?;
    let _ = context.flush();
    let (_connection, events) = context
        .handshake_tokio("overdesk", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| e.to_string())?;
    let mut events = pin!(events);
    // Subscribe before enabling so no activation is missed.
    let mut activated = pin!(ic.receive_activated().await.map_err(|e| e.to_string())?);
    let mut deactivated = pin!(ic.receive_deactivated().await.map_err(|e| e.to_string())?);
    let mut disabled = pin!(ic.receive_disabled().await.map_err(|e| e.to_string())?);
    let mut zones_changed = pin!(ic.receive_zones_changed().await.map_err(|e| e.to_string())?);

    let mut machine = Machine::default();
    machine.step_into(0, Input::Armed(arm(ic, session).await?), &mut Vec::new());
    if let Some(r) = ready.take() {
        let _ = r.send(Ok(()));
    }
    let start = Instant::now();
    let mut pending = Vec::new();
    loop {
        // Only an unclaimed activation times out; otherwise the loop sleeps until an event.
        let deadline = machine.next_deadline().map(|ms| start + Duration::from_millis(ms));
        let input = tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => convert(ev, &context),
                _ => Some(Input::Failed),
            },
            Some(a) = activated.next() => a.activation_id().map(|id| Input::Activated {
                id,
                pos: a.cursor_position().unwrap_or((0.0, 0.0)),
                barrier: match a.barrier_id() {
                    Some(ActivatedBarrier::Barrier(b)) => Some(b.get()),
                    _ => None,
                },
            }),
            Some(d) = deactivated.next() => Some(Input::Deactivated(d.activation_id())),
            Some(_) = disabled.next() => Some(Input::Disabled),
            Some(_) = zones_changed.next() => {
                // GNOME needs the session disabled before barriers can change.
                let _ = ic.disable(session, Default::default()).await;
                Some(Input::ZonesChanged)
            }
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Swallow(on)) => Some(Input::Swallow(on)),
                None => return Ok(()),
            },
            () = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => Some(Input::Tick),
        };
        let Some(input) = input else { continue };
        let failed = input == Input::Failed;
        let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        machine.step_into(now, input, &mut pending);
        for out in pending.drain(..) {
            match out {
                Out::Emit(c) => {
                    if tx.send(c).is_err() {
                        return Ok(());
                    }
                }
                Out::Release { id, at, local } => {
                    with_pointer(|p| p.set(local));
                    let opts = ReleaseOptions::default().set_activation_id(id).set_cursor_position(at);
                    ic.release(session, opts).await.map_err(|e| e.to_string())?;
                }
                Out::Rearm => {
                    // Arming again produces no output, only the new zones.
                    machine.step_into(now, Input::Armed(arm(ic, session).await?), &mut Vec::new());
                }
            }
        }
        if failed {
            return Err("the portal session or EIS connection closed".into());
        }
    }
}

/// One request to the injection task.
enum Msg {
    /// Relative pointer motion.
    Motion(f64, f64),
    /// Button edge, evdev code.
    Button(i32, bool),
    /// Key edge, evdev code.
    Key(i32, bool),
    /// Smooth scroll, positive down and right.
    Smooth(f64, f64),
    /// Discrete wheel steps.
    Steps(bool, i32),
}

/// Folds `next` into `msg` when both are the same kind of motion (their deltas add up, which
/// is the same motion delivered once). Gives `next` back when it cannot be folded.
fn merge(msg: &mut Msg, next: Msg) -> Option<Msg> {
    match (msg, next) {
        (Msg::Motion(x, y), Msg::Motion(dx, dy)) | (Msg::Smooth(x, y), Msg::Smooth(dx, dy)) => {
            *x += dx;
            *y += dy;
            None
        }
        (_, other) => Some(other),
    }
}

/// Injects input through the RemoteDesktop portal.
pub struct Injector {
    /// Requests to the injection task; closing it ends the task and its session.
    tx: UnboundedSender<Msg>,
    /// Signalled when the task has finished.
    done: Receiver<()>,
    /// Keys pressed and not yet released (Overdesk codes).
    keys: Pressed<u32>,
    /// Buttons pressed and not yet released.
    buttons: Pressed<Button>,
    /// Carries the fraction of discrete wheel steps between events.
    scroll: Notches,
    /// Carries the fraction of a Ctrl+wheel step between pinch events.
    pinch: Notches,
    /// Scratch for the moves of one `pointer` call, reused so a move allocates nothing.
    moves: Vec<(f64, f64)>,
    /// Scratch for the portal scrolls of one `scroll` call.
    scrolls: Vec<ScrollOut>,
}

impl Injector {
    /// Creates the RemoteDesktop session. The first run shows the compositor's consent
    /// dialog; the restore token, kept in the store folder `token_dir`, makes later runs
    /// silent. What goes wrong later is reported to `warnings`. Blocks until it is ready.
    pub fn new(token_dir: &Path, warnings: Warnings) -> Result<Self, LinuxError> {
        let (tx, rx) = unbounded_channel();
        let dir = token_dir.to_path_buf();
        let reported = warnings.clone();
        let done = start_portal_task("injection", warnings, move |ready| {
            Box::pin(async move { inject(rx, ready, &dir, &reported).await })
        })?;
        Ok(Self {
            tx,
            done,
            keys: Pressed::default(),
            buttons: Pressed::default(),
            scroll: Notches::default(),
            pinch: Notches::default(),
            moves: Vec::new(),
            scrolls: Vec::new(),
        })
    }

    /// Where the pointer is according to what this adapter has done; `(0, 0)` if unknown.
    pub fn cursor() -> (f64, f64) {
        with_pointer(|p| p.get()).unwrap_or((0.0, 0.0))
    }

    /// Moves the pointer to desk-box pixel coordinates, as relative motion from where
    /// it was last known to be.
    pub fn pointer(&mut self, x: f64, y: f64) {
        with_pointer(|p| p.move_to(x, y, &mut self.moves));
        for (dx, dy) in self.moves.drain(..) {
            let _ = self.tx.send(Msg::Motion(dx, dy));
        }
    }

    /// Presses or releases `button`, tracking it for [`Injector::release_all`].
    pub fn button(&mut self, button: Button, pressed: bool) {
        self.buttons.set(button, pressed);
        let code = i32::try_from(evdev_from_button(button)).unwrap_or(0);
        let _ = self.tx.send(Msg::Button(code, pressed));
    }

    /// Presses or releases a key; `false` for codes Linux has no key for.
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        let Some(keycode) = code_to_keycode(code) else {
            return false;
        };
        self.keys.set(code, pressed);
        // The keymap holds X11 keycodes, which are evdev codes plus 8.
        let _ = self.tx.send(Msg::Key(i32::from(keycode) - 8, pressed));
        true
    }

    /// Scrolls: trackpad pixels smoothly, wheel notches as whole discrete steps. The portal
    /// has no scroll phases, so `_phase` is ignored.
    pub fn scroll(&mut self, dx: i32, dy: i32, unit: ScrollUnit, _phase: Phase) {
        scroll_out(&mut self.scroll, dx, dy, unit, &mut self.scrolls);
        for out in self.scrolls.drain(..) {
            let _ = self.tx.send(match out {
                ScrollOut::Smooth(x, y) => Msg::Smooth(x, y),
                ScrollOut::Steps { horizontal, steps } => Msg::Steps(horizontal, steps),
            });
        }
    }

    /// Replays a pinch as Ctrl+wheel steps (the zoom shortcut of browsers and editors);
    /// rotate and smart zoom have no public portal equivalent and are ignored.
    pub fn gesture(&mut self, gesture: Gesture, _phase: Phase) {
        let Some(wheel) = pinch_wheel(gesture) else { return };
        let (_, steps) = self.pinch.feed(0, wheel, ScrollUnit::Notch120);
        if steps == 0 {
            return;
        }
        let held = self.keys.contains(&CTRL_KEY);
        if !held {
            self.key(CTRL_KEY, true);
        }
        // The portal's discrete axis is positive down; pinch-out (positive) is wheel up.
        let _ = self.tx.send(Msg::Steps(false, (-steps).clamp(-MAX_STEPS, MAX_STEPS)));
        if !held {
            self.key(CTRL_KEY, false);
        }
    }

    /// Types `chord` (a desktop shortcut such as Super+Page Down) through the tracked key path,
    /// so [`Self::release_all`] covers it; a key the remote user already holds is left alone.
    pub fn chord(&mut self, chord: &[u32]) {
        for (code, pressed) in chord_events(chord, |c| self.keys.contains(&c)) {
            self.key(code, pressed);
        }
    }

    /// Releases every key and button this injector holds down.
    pub fn release_all(&mut self) {
        for code in self.keys.snapshot().into_iter().rev() {
            self.key(code, false);
        }
        for button in self.buttons.snapshot() {
            self.button(button, false);
        }
    }
}

/// Stuck keys would outlive the session otherwise: release them and let the task flush.
impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
        let (dead, _) = unbounded_channel();
        drop(std::mem::replace(&mut self.tx, dead));
        let _ = self.done.recv_timeout(DROP_WAIT);
    }
}

/// The injection task: creates the RemoteDesktop session and replays requests until the
/// adapter is dropped or the session fails.
async fn inject(
    mut rx: UnboundedReceiver<Msg>,
    ready: &mut Ready,
    dir: &Path,
    warnings: &Warnings,
) -> Result<(), String> {
    register().await;
    let rd = RemoteDesktop::new().await.map_err(|e| e.to_string())?;
    let session = rd.create_session(Default::default()).await.map_err(|e| e.to_string())?;
    let wanted = DeviceType::KEYBOARD | DeviceType::POINTER;
    let token = load_token(dir);
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(wanted)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await
    .map_err(|e| e.to_string())?
    .response()
    .map_err(|e| e.to_string())?;
    let started = rd
        .start(&session, None, Default::default())
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;
    // Single use: the token just spent is dead, so keep the fresh one.
    if let Some(t) = started.restore_token()
        && let Err(e) = save_token(dir, t)
    {
        warnings.raise(Warning::RestoreTokenNotSaved(e.to_string()));
    }
    if !started.devices().contains(wanted) {
        let _ = session.close().await;
        return Err("consent was given without keyboard and pointer control".into());
    }
    if let Some(r) = ready.take() {
        let _ = r.send(Ok(()));
    }
    let result = async {
        // A message taken from the queue while folding that turned out not to fold.
        let mut carry = None;
        loop {
            let mut msg = match carry.take() {
                Some(m) => m,
                None => match rx.recv().await {
                    Some(m) => m,
                    None => break,
                },
            };
            // If the portal falls behind, a run of motion or smooth-scroll messages is one
            // message with the summed delta: fewer round trips, the same distance.
            while let Ok(next) = rx.try_recv() {
                if let Some(other) = merge(&mut msg, next) {
                    carry = Some(other);
                    break;
                }
            }
            let state = |p: bool| if p { KeyState::Pressed } else { KeyState::Released };
            match msg {
                Msg::Motion(dx, dy) => rd.notify_pointer_motion(&session, dx, dy, Default::default()).await,
                Msg::Button(b, p) => {
                    rd.notify_pointer_button(&session, b, state(p), Default::default())
                        .await
                }
                Msg::Key(k, p) => {
                    rd.notify_keyboard_keycode(&session, k, state(p), Default::default())
                        .await
                }
                Msg::Smooth(dx, dy) => {
                    rd.notify_pointer_axis(
                        &session,
                        dx,
                        dy,
                        ashpd::desktop::remote_desktop::NotifyPointerAxisOptions::default(),
                    )
                    .await
                }
                Msg::Steps(horizontal, steps) => {
                    let axis = if horizontal { Axis::Horizontal } else { Axis::Vertical };
                    rd.notify_pointer_axis_discrete(&session, axis, steps, Default::default())
                        .await
                }
            }
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    .await;
    let _ = session.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_of_motion_and_of_smooth_scroll_fold_but_nothing_else_does() {
        let mut m = Msg::Motion(1.0, 2.0);
        assert!(merge(&mut m, Msg::Motion(3.0, -1.0)).is_none());
        assert!(matches!(m, Msg::Motion(x, y) if x == 4.0 && y == 1.0));
        assert!(matches!(
            merge(&mut m, Msg::Button(0x110, true)),
            Some(Msg::Button(0x110, true))
        ));
        assert!(
            matches!(merge(&mut m, Msg::Smooth(1.0, 1.0)), Some(Msg::Smooth(..))),
            "different kinds stay apart"
        );
        let mut s = Msg::Smooth(0.5, 0.0);
        assert!(merge(&mut s, Msg::Smooth(0.5, 2.0)).is_none());
        assert!(matches!(s, Msg::Smooth(x, y) if x == 1.0 && y == 2.0));
    }
}
