// SPDX-License-Identifier: AGPL-3.0-only
//! Wayland capture and injection through the desktop portals (design: `docs/WAYLAND.md`).
//!
//! Capture uses `InputCapture`: barriers on the primary screen's outer edges, and while the
//! compositor has capture active, input arrives over EIS (`reis`, receiver side). Injection
//! uses `RemoteDesktop` with the `Notify*` calls. One tokio thread owns every portal session;
//! the adapters talk to it over channels. All decisions live in `wayland_core`.
//!
//! shortcut: injection uses the portal's D-Bus `Notify*` methods, not `ConnectToEIS`: same
//! consent and codes, no sender-side EIS device handshake. Move to EIS if per-event D-Bus
//! latency shows up on hardware.

use std::collections::VecDeque;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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
use overdesk_proto::{Button, Phase, ScrollUnit};
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::events::Notches;
use super::ffi::LinuxError;
use super::keymap::code_to_keycode;
use super::wayland_core::{
    Input, Machine, Out, Pointer, ScrollOut, Zone, evdev_from_button, plan_barriers, portal_unavailable, primary_zone,
    scroll_out, store_dir,
};
use crate::input::Captured;

/// The id Overdesk registers with the portal as an unsandboxed app (basename of its
/// `.desktop` file); see `docs/WAYLAND.md`.
const APP_ID: &str = "org.overdesk.Overdesk";
/// File in the store directory that keeps the RemoteDesktop restore token.
const TOKEN_FILE: &str = "wayland-restore-token";
/// How often the capture loop ticks the arbitration timer.
const TICK: Duration = Duration::from_millis(50);
/// How long dropping an adapter waits for its portal task to finish releasing.
const DROP_WAIT: Duration = Duration::from_secs(1);

/// Primary screen size as read from the portal zones; `None` before capture armed.
static PRIMARY: Mutex<Option<(u32, u32)>> = Mutex::new(None);
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

/// Primary display size in pixels; `(0, 0)` until capture has read the zones.
pub fn main_display_size() -> (u32, u32) {
    PRIMARY.lock().unwrap_or_else(PoisonError::into_inner).unwrap_or((0, 0))
}

/// Registers this unsandboxed process with the portal under [`APP_ID`], so the portal can
/// key permissions and restore tokens to Overdesk. At most once per connection and only for
/// unsandboxed apps: a repeat or a refusal (older portals) is harmless and ignored.
async fn register() {
    if let Ok(id) = AppID::try_from(APP_ID) {
        let _ = ashpd::register_host_app(id).await;
    }
}

/// Path of the stored RemoteDesktop restore token.
fn token_path() -> PathBuf {
    store_dir(std::env::var_os("OVERDESK_HOME"), std::env::var_os("HOME")).join(TOKEN_FILE)
}

/// The saved restore token, if any.
fn load_token() -> Option<String> {
    let t = std::fs::read_to_string(token_path()).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// Saves the restore token with owner-only permissions. Tokens are single use, so this runs
/// after every start.
fn save_token(token: &str) -> std::io::Result<()> {
    let path = token_path();
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
struct EscapeOnDrop(Sender<Captured>);

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
    /// Creates the InputCapture session, sets barriers and enables capture. Blocks until it
    /// is armed (the compositor may ask for consent first). Errors say the portal is
    /// unavailable and why X11 is not used instead.
    pub fn start() -> Result<(Self, Receiver<Captured>), LinuxError> {
        let (tx, rx) = channel();
        let (ready_tx, ready) = channel();
        let (cmds, cmd_rx) = unbounded_channel();
        let (done_tx, done) = channel();
        spawn_portal(move || async move {
            let guard = EscapeOnDrop(tx.clone());
            let mut ready_tx = Some(ready_tx);
            if let Err(e) = capture(&tx, cmd_rx, &mut ready_tx).await {
                match ready_tx.take() {
                    Some(r) => {
                        let _ = r.send(Err(e));
                    }
                    None => eprintln!("Wayland capture ended: {e}"),
                }
            }
            drop(guard);
            let _ = done_tx.send(());
        })?;
        ready
            .recv()
            .map_err(|_| unavailable("portal task stopped"))?
            .map_err(unavailable)?;
        Ok((
            Self {
                cmds,
                done: Mutex::new(done),
            },
            rx,
        ))
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

/// Reads the portal zones, sets barriers on the primary screen and enables capture.
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
    let primary = primary_zone(&list).ok_or("the compositor reported no screens")?;
    let barriers: Vec<Barrier> = plan_barriers(primary, &list)
        .into_iter()
        .filter_map(|b| {
            let id = std::num::NonZeroU32::new(b.edge.barrier_id())?;
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
async fn capture(
    tx: &Sender<Captured>,
    mut cmds: UnboundedReceiver<Cmd>,
    ready: &mut Option<Sender<Result<(), String>>>,
) -> Result<(), String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    register().await;
    let ic = InputCapture::new().await.map_err(|e| s(&e))?;
    let (session, _caps) = ic
        .create_session(
            None,
            CreateSessionOptions::default().set_capabilities(Capabilities::KEYBOARD | Capabilities::POINTER),
        )
        .await
        .map_err(|e| s(&e))?;
    let result = capture_session(tx, &mut cmds, ready, &ic, &session).await;
    let _ = session.close().await;
    result
}

/// The body of [`capture`], split out so the session is always closed afterwards.
async fn capture_session(
    tx: &Sender<Captured>,
    cmds: &mut UnboundedReceiver<Cmd>,
    ready: &mut Option<Sender<Result<(), String>>>,
    ic: &InputCapture,
    session: &ashpd::desktop::Session<InputCapture>,
) -> Result<(), String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    let fd = ic
        .connect_to_eis(session, Default::default())
        .await
        .map_err(|e| s(&e))?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true).map_err(|e| s(&e))?;
    let context = ei::Context::new(stream).map_err(|e| s(&e))?;
    let _ = context.flush();
    let (_connection, events) = context
        .handshake_tokio("overdesk", ei::handshake::ContextType::Receiver)
        .await
        .map_err(|e| s(&e))?;
    let mut events = pin!(events);
    // Subscribe before enabling so no activation is missed.
    let mut activated = pin!(ic.receive_activated().await.map_err(|e| s(&e))?);
    let mut deactivated = pin!(ic.receive_deactivated().await.map_err(|e| s(&e))?);
    let mut disabled = pin!(ic.receive_disabled().await.map_err(|e| s(&e))?);
    let mut zones_changed = pin!(ic.receive_zones_changed().await.map_err(|e| s(&e))?);

    let mut machine = Machine::default();
    machine.step(0, Input::Armed(arm(ic, session).await?));
    if let Some(r) = ready.take() {
        let _ = r.send(Ok(()));
    }
    let start = Instant::now();
    let mut tick = tokio::time::interval(TICK);
    loop {
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
            _ = tick.tick() => Some(Input::Tick),
        };
        let Some(input) = input else { continue };
        let failed = input == Input::Failed;
        let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut pending: VecDeque<Out> = machine.step(now, input).into();
        while let Some(out) = pending.pop_front() {
            match out {
                Out::Emit(c) => {
                    if tx.send(c).is_err() {
                        return Ok(());
                    }
                }
                Out::Release { id, at, local } => {
                    with_pointer(|p| p.set(local));
                    let opts = ReleaseOptions::default().set_activation_id(id).set_cursor_position(at);
                    ic.release(session, opts).await.map_err(|e| s(&e))?;
                }
                Out::Rearm => {
                    let zones = arm(ic, session).await?;
                    pending.extend(machine.step(now, Input::Armed(zones)));
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

/// Injects input through the RemoteDesktop portal.
pub struct Injector {
    /// Requests to the injection task; closing it ends the task and its session.
    tx: UnboundedSender<Msg>,
    /// Signalled when the task has finished.
    done: Receiver<()>,
    /// Keys pressed and not yet released (Overdesk codes).
    keys: Vec<u32>,
    /// Buttons pressed and not yet released.
    buttons: Vec<Button>,
    /// Carries the fraction of discrete wheel steps between events.
    scroll: Notches,
}

impl Injector {
    /// Creates the RemoteDesktop session. The first run shows the compositor's consent
    /// dialog; the restore token makes later runs silent. Blocks until it is ready.
    pub fn new() -> Result<Self, LinuxError> {
        let (tx, rx) = unbounded_channel();
        let (ready_tx, ready) = channel();
        let (done_tx, done) = channel();
        spawn_portal(move || async move {
            let mut ready_tx = Some(ready_tx);
            if let Err(e) = inject(rx, &mut ready_tx).await {
                match ready_tx.take() {
                    Some(r) => {
                        let _ = r.send(Err(e));
                    }
                    None => eprintln!("Wayland injection ended: {e}"),
                }
            }
            let _ = done_tx.send(());
        })?;
        ready
            .recv()
            .map_err(|_| unavailable("portal task stopped"))?
            .map_err(unavailable)?;
        Ok(Self {
            tx,
            done,
            keys: Vec::new(),
            buttons: Vec::new(),
            scroll: Notches::default(),
        })
    }

    /// Where the pointer is according to what this adapter has done; `(0, 0)` if unknown.
    pub fn cursor() -> (f64, f64) {
        with_pointer(|p| p.get()).unwrap_or((0.0, 0.0))
    }

    /// Moves the pointer to primary-screen pixel coordinates, as relative motion from where
    /// it was last known to be.
    pub fn pointer(&mut self, x: f64, y: f64) {
        for (dx, dy) in with_pointer(|p| p.move_to(x, y)) {
            let _ = self.tx.send(Msg::Motion(dx, dy));
        }
    }

    /// Presses or releases `button`, tracking it for [`Injector::release_all`].
    pub fn button(&mut self, button: Button, pressed: bool) {
        self.buttons.retain(|b| *b != button);
        if pressed {
            self.buttons.push(button);
        }
        let code = i32::try_from(evdev_from_button(button)).unwrap_or(0);
        let _ = self.tx.send(Msg::Button(code, pressed));
    }

    /// Presses or releases a key; `false` for codes Linux has no key for.
    pub fn key(&mut self, code: u32, pressed: bool) -> bool {
        let Some(keycode) = code_to_keycode(code) else {
            return false;
        };
        self.keys.retain(|k| *k != code);
        if pressed {
            self.keys.push(code);
        }
        // The keymap holds X11 keycodes, which are evdev codes plus 8.
        let _ = self.tx.send(Msg::Key(i32::from(keycode) - 8, pressed));
        true
    }

    /// Scrolls: trackpad pixels smoothly, wheel notches as whole discrete steps. The portal
    /// has no scroll phases, so `_phase` is ignored.
    pub fn scroll(&mut self, dx: i32, dy: i32, unit: ScrollUnit, _phase: Phase) {
        for out in scroll_out(&mut self.scroll, dx, dy, unit) {
            let _ = self.tx.send(match out {
                ScrollOut::Smooth(x, y) => Msg::Smooth(x, y),
                ScrollOut::Steps { horizontal, steps } => Msg::Steps(horizontal, steps),
            });
        }
    }

    /// Releases every key and button this injector holds down.
    pub fn release_all(&mut self) {
        for code in std::mem::take(&mut self.keys).into_iter().rev() {
            self.key(code, false);
        }
        for button in std::mem::take(&mut self.buttons) {
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
async fn inject(mut rx: UnboundedReceiver<Msg>, ready: &mut Option<Sender<Result<(), String>>>) -> Result<(), String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    register().await;
    let rd = RemoteDesktop::new().await.map_err(|e| s(&e))?;
    let session = rd.create_session(Default::default()).await.map_err(|e| s(&e))?;
    let wanted = DeviceType::KEYBOARD | DeviceType::POINTER;
    let token = load_token();
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(wanted)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await
    .map_err(|e| s(&e))?
    .response()
    .map_err(|e| s(&e))?;
    let started = rd
        .start(&session, None, Default::default())
        .await
        .map_err(|e| s(&e))?
        .response()
        .map_err(|e| s(&e))?;
    // Single use: the token just spent is dead, so keep the fresh one.
    if let Some(t) = started.restore_token()
        && let Err(e) = save_token(t)
    {
        eprintln!("Wayland: could not save the restore token: {e}");
    }
    if !started.devices().contains(wanted) {
        let _ = session.close().await;
        return Err("consent was given without keyboard and pointer control".into());
    }
    if let Some(r) = ready.take() {
        let _ = r.send(Ok(()));
    }
    let result = async {
        while let Some(msg) = rx.recv().await {
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
