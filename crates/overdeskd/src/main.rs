// SPDX-License-Identifier: AGPL-3.0-only
//! `overdeskd`: run one Overdesk peer.
//!
//!   overdeskd listen [ADDR] [--pair] [--headless] [--name NAME]
//!   overdeskd connect ADDR  [--pair] [--headless] [--name NAME] [--left]
//!
//! `--pair` accepts an unknown device after both users confirm the 6-digit code.
//! `--headless` prints what would be injected instead of touching input (for testing).
//! `--exit-after SECS` quits unconditionally after SECS: a safety net while testing,
//! since process exit always returns input to this device.
//! `OVERDESK_HOME` overrides where the identity and pinned peers are stored.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use overdesk_core::engine::Action;
use overdesk_net::session::{Me, pair};
use overdesk_net::store::Store;
use overdesk_net::transport::{Link, Node};
use overdesk_platform::input::Captured;
use overdesk_proto::Os;
use overdeskd::{Platform, Side, establish, run};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

const DEFAULT_PORT: u16 = 24801;

struct Args {
    listen: Option<SocketAddr>,
    connect: Option<SocketAddr>,
    pair: bool,
    headless: bool,
    side: Side,
    name: String,
    exit_after: Option<u64>,
}

fn parse() -> Result<Args, String> {
    let mut a = std::env::args().skip(1);
    let default_listen = SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT));
    let mut args = Args {
        listen: None,
        connect: None,
        pair: false,
        headless: false,
        side: Side::Right,
        name: std::env::var("HOSTNAME").unwrap_or_else(|_| "overdesk".into()),
        exit_after: None,
    };
    let addr = |s: Option<String>| {
        s.ok_or("missing address")?
            .parse::<SocketAddr>()
            .map_err(|e| e.to_string())
    };
    match a.next().as_deref() {
        Some("listen") => args.listen = Some(default_listen),
        Some("connect") => args.connect = Some(addr(a.next())?),
        _ => {
            return Err(
                "usage: overdeskd listen [ADDR] | connect ADDR [--pair] [--headless] [--left] [--name N]".into(),
            );
        }
    }
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "--pair" => args.pair = true,
            "--headless" => args.headless = true,
            "--left" => args.side = Side::Left,
            "--name" => args.name = a.next().ok_or("--name needs a value")?,
            "--exit-after" => {
                let secs = a.next().ok_or("--exit-after needs seconds")?;
                args.exit_after = Some(secs.parse().map_err(|e| format!("--exit-after {secs}: {e}"))?);
            }
            other if args.listen.is_some() => args.listen = Some(other.parse().map_err(|e| format!("{other}: {e}"))?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

fn store_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OVERDESK_HOME") {
        return dir.into();
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Overdesk")
    } else {
        home.join(".overdesk")
    }
}

/// Capture events go to whichever session is running now.
type Slot = Arc<Mutex<Option<UnboundedSender<Captured>>>>;

#[derive(Clone)]
struct Headless;

impl Platform for Headless {
    fn swallow(&self, on: bool) {
        println!("[headless] swallow {on}");
    }
    fn inject(&self, action: Action) {
        println!("[headless] {action:?}");
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::Arc;
    use std::sync::mpsc::{Sender, channel};

    use overdesk_core::engine::Action;
    use overdesk_platform::mac::ffi::{Capture, Injector};

    use super::Slot;

    #[derive(Clone)]
    pub struct MacPlatform {
        capture: Arc<Capture>,
        inject: Sender<Action>,
    }

    impl MacPlatform {
        pub fn start(slot: Slot) -> Result<Self, String> {
            let (capture, events) = Capture::start().map_err(|e| format!("capture: {e:?}"))?;
            std::thread::Builder::new()
                .name("overdesk-capture-bridge".into())
                .spawn(move || {
                    while let Ok(c) = events.recv() {
                        if let Some(tx) = slot.lock().expect("slot lock").as_ref() {
                            let _ = tx.send(c);
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            let (inject, actions) = channel::<Action>();
            let (ready_tx, ready) = channel::<Result<(), String>>();
            // The Quartz event source is not Send: create and use it on one thread.
            std::thread::Builder::new()
                .name("overdesk-inject".into())
                .spawn(move || {
                    let mut injector = match Injector::new() {
                        Ok(i) => {
                            let _ = ready_tx.send(Ok(()));
                            i
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("inject: {e:?}")));
                            return;
                        }
                    };
                    while let Ok(a) = actions.recv() {
                        match a {
                            Action::InjectMove { x, y } => injector.pointer(x as f64, y as f64),
                            Action::InjectButton { button, pressed } => injector.button(button, pressed),
                            Action::InjectKey { code, pressed } => {
                                injector.key(code, pressed);
                            }
                            Action::InjectScroll { dx, dy, unit, phase } => injector.scroll(dx, dy, unit, phase),
                            Action::ReleaseAll => injector.release_all(),
                            _ => {}
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            ready.recv().map_err(|e| e.to_string())??;
            Ok(Self {
                capture: Arc::new(capture),
                inject,
            })
        }
    }

    impl super::Platform for MacPlatform {
        fn swallow(&self, on: bool) {
            self.capture.set_swallow(on);
        }
        fn inject(&self, action: Action) {
            let _ = self.inject.send(action);
        }
    }
}

async fn ask(code: u32) -> bool {
    println!(
        "Pairing code: {:03} {:03}. Does the other screen show the same? [y/N]",
        code / 1000,
        code % 1000
    );
    let mut line = String::new();
    let _ = BufReader::new(tokio::io::stdin()).read_line(&mut line).await;
    line.trim().eq_ignore_ascii_case("y")
}

/// Pins or pairs, then runs one session to its end.
async fn session(mut link: Link, store: &Store, args: &Args, me: &Me, platform: impl Platform, slot: &Slot) {
    let peer = link.peer_fingerprint();
    let pinned = store.pins().unwrap_or_default();
    if !pinned.contains(&peer) {
        if !args.pair {
            println!("Unknown device; re-run with --pair to pair it.");
            link.close("not paired");
            return;
        }
        match pair(&mut link, ask).await {
            Ok(fp) => {
                if let Err(e) = store.pin(fp) {
                    eprintln!("could not save the pairing: {e}");
                    return;
                }
                println!("Paired.");
            }
            Err(e) => {
                println!("Pairing failed: {e}");
                return;
            }
        }
    }
    let pins = store.pins().unwrap_or_default();
    match establish(link, |fp| pins.contains(fp), me, args.side).await {
        Ok(s) => {
            println!("Connected to {}. Escape hatch: Ctrl+Option+Cmd+Esc.", s.peer_name);
            let (tx, rx) = unbounded_channel();
            *slot.lock().expect("slot lock") = Some(tx);
            run(s, rx, platform).await;
            *slot.lock().expect("slot lock") = None;
            println!("Disconnected.");
        }
        Err(e) => println!("Session failed: {e}"),
    }
}

async fn serve(args: Args, platform: impl Platform + Clone, slot: Slot, me: Me) -> Result<(), String> {
    let store = Store::open(store_dir()).map_err(|e| e.to_string())?;
    let id = store.identity().map_err(|e| e.to_string())?;
    let bind = args.listen.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let node = Node::bind(&id, bind).map_err(|e| e.to_string())?;
    let fp: String = id.fingerprint()[..4].iter().map(|b| format!("{b:02x}")).collect();
    println!(
        "Overdesk device {fp}… ({}x{}) on {}",
        me.width,
        me.height,
        node.local_addr().map_err(|e| e.to_string())?
    );
    if let Some(addr) = args.connect {
        let link = node.connect(addr).await.map_err(|e| e.to_string())?;
        session(link, &store, &args, &me, platform, &slot).await;
        return Ok(());
    }
    // shortcut: one session at a time (Free tier: 2 devices); others wait for the next accept.
    while let Some(setup) = node.accept().await {
        match setup.await {
            Ok(link) => session(link, &store, &args, &me, platform.clone(), &slot).await,
            Err(e) => println!("Incoming connection failed: {e}"),
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if let Some(secs) = args.exit_after {
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            println!("--exit-after {secs}s reached; exiting.");
            std::process::exit(0);
        });
    }
    let slot: Slot = Arc::default();
    let result = if args.headless {
        let me = Me {
            name: args.name.clone(),
            os: Os::Linux,
            width: 1920,
            height: 1080,
        };
        serve(args, Headless, slot, me).await
    } else {
        start_native(args, slot).await
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
async fn start_native(args: Args, slot: Slot) -> Result<(), String> {
    let (width, height) = overdesk_platform::mac::ffi::main_display_size();
    let me = Me {
        name: args.name.clone(),
        os: Os::MacOs,
        width,
        height,
    };
    let platform = mac::MacPlatform::start(slot.clone())?;
    serve(args, platform, slot, me).await
}

#[cfg(not(target_os = "macos"))]
async fn start_native(_args: Args, _slot: Slot) -> Result<(), String> {
    Err("no input adapter for this OS yet; use --headless".into())
}
