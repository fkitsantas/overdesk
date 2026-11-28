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

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use overdesk_core::engine::Action;
use overdesk_net::pairing::peer_id;
use overdesk_net::session::Me;
use overdesk_net::store::Store;
use overdesk_net::transport::{CloseReason, Link, Node};
use overdesk_proto::{Os, log};
use overdeskd::controller::trust::{Direction, PairingWindow, PinBook, Unpaired, pair_with_user};
use overdeskd::controller::{Phase, session_me};
use overdeskd::{
    Clipboard, Platform, Side, escape_chord, establish, load_identity, redact_action, run, store_dir, this_os,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::Instant;

/// The port `listen` binds to when no address is given.
const DEFAULT_PORT: u16 = 24801;

/// The parsed command line.
struct Args {
    /// The address to accept connections on, when running `listen`.
    listen: Option<SocketAddr>,
    /// The address to dial, when running `connect`.
    connect: Option<SocketAddr>,
    /// Whether an unknown device may pair after both users confirm the code.
    pair: bool,
    /// Whether to print actions instead of touching real input.
    headless: bool,
    /// Where the peer's screen sits; only the dialer's choice counts.
    side: Side,
    /// This device's name, as shown to the peer.
    name: String,
    /// Quit unconditionally after this many seconds, if set.
    exit_after: Option<u64>,
}

/// Parses the command line, returning a usage message on error.
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
                "usage: overdeskd listen [ADDR] | connect ADDR [--pair] [--headless] [--left] [--name N] [--exit-after SECS]"
                    .into(),
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
            // After `listen`, a bare argument is the address to bind.
            other if args.listen.is_some() => args.listen = Some(other.parse().map_err(|e| format!("{other}: {e}"))?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

/// A [`Platform`] that prints what it would do, for testing without real input.
#[derive(Clone)]
struct Headless;

impl Clipboard for Headless {}

impl Platform for Headless {
    fn swallow(&self, on: bool) {
        println!("[headless] swallow {on}");
    }
    fn inject(&self, action: Action) {
        println!("[headless] {:?}", redact_action(&action));
    }
}

/// Shows the pairing code and, in the background, reads the user's yes or no from stdin into `window`.
fn ask(window: &Arc<PairingWindow>, code: u32) {
    println!(
        "Pairing code: {:03} {:03}. Does the other screen show the same? [y/N]",
        code / 1000,
        code % 1000
    );
    let window = window.clone();
    tokio::spawn(async move {
        let mut line = String::new();
        // An unreadable stdin is a "no".
        let _ = BufReader::new(tokio::io::stdin()).read_line(&mut line).await;
        window.answer(Some(code), line.trim().eq_ignore_ascii_case("y"));
    });
}

/// What a session needs besides its link.
struct Setup<'a, P> {
    /// The command line.
    args: &'a Args,
    /// The pinned peers.
    book: &'a PinBook,
    /// Whether, and for how long, a stranger may pair.
    window: &'a Arc<PairingWindow>,
    /// This device, as announced to peers.
    me: &'a Me,
    /// The input adapter.
    platform: P,
}

/// Pins or pairs, then runs one session to its end. An error says why it never started.
async fn session<P: Platform + Sync>(mut link: Link, direction: Direction, setup: Setup<'_, P>) -> Result<(), String> {
    let Setup {
        args,
        book,
        window,
        me,
        platform,
    } = setup;
    if !book.load().0.contains(&link.peer_fingerprint()) {
        if !args.pair {
            link.close(CloseReason::NotPaired);
            return Err("Unknown device; re-run with --pair to pair it.".into());
        }
        let show = |phase: Phase| {
            if let Phase::Pairing { code, .. } = phase {
                ask(window, code);
            }
        };
        match pair_with_user(&mut link, direction, book, window, show, || ()).await {
            Ok(()) => println!("Paired."),
            Err(Unpaired::Refused) => return Err("Pairing is not open right now.".into()),
            Err(Unpaired::Failed(why)) => return Err(why),
        }
    }
    let pins = book.load().0;
    // The display may have changed since this process started.
    let now = session_me(me, platform.display_size());
    match establish(link, |fp| pins.contains(fp), &now, args.side, None).await {
        Ok(s) => {
            println!("Connected to {}. Escape hatch: {}.", s.peer_name, escape_chord(me.os));
            let local = platform.capture();
            run(s, local, platform, |_| {}).await;
            println!("Disconnected.");
            Ok(())
        }
        Err(e) => Err(format!("Session failed: {e}")),
    }
}

/// Opens the store, binds a node, then dials once or accepts sessions until stopped.
async fn serve(args: Args, platform: impl Platform + Clone + Sync, me: Me) -> Result<(), String> {
    let dir = store_dir();
    let store = Store::open(&dir).map_err(|e| e.to_string())?;
    let id = load_identity(&store).map_err(|e| e.to_string())?;
    let book = PinBook::new(dir, Arc::new(store));
    let window = Arc::new(PairingWindow::default());
    if args.pair {
        // `--pair` is the user opening pairing for as long as this process runs.
        window.set(Some(Instant::now() + Duration::from_secs(365 * 24 * 3600)));
    }
    let bind = args.listen.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let node = Node::bind(&id, bind).map_err(|e| e.to_string())?;
    let fp = overdesk_proto::hex::hex(&id.fingerprint()[..4]);
    println!(
        "Overdesk device {fp}… ({}x{}) on {}",
        me.width,
        me.height,
        node.local_addr().map_err(|e| e.to_string())?
    );
    if let Some(addr) = args.connect {
        let link = node.connect(addr).await.map_err(|e| e.to_string())?;
        let setup = Setup {
            args: &args,
            book: &book,
            window: &window,
            me: &me,
            platform,
        };
        return session(link, Direction::Outbound, setup).await;
    }
    // Announce this listener on the LAN so Overdesk apps list it under Nearby computers.
    // Discovery is a convenience: if multicast is unavailable, listening still works.
    let port = node.local_addr().map_err(|e| e.to_string())?.port();
    let _advertiser = overdesk_net::discovery::Advertiser::start(peer_id(&id.fingerprint()), &me.name, me.os, port)
        .map_err(|e| log::warn(format_args!("LAN discovery unavailable: {e}")))
        .ok();
    // shortcut: one session at a time (Free tier: 2 devices); others wait for the next accept.
    while let Some(setup) = node.accept().await {
        match setup.await {
            Ok(link) => {
                // A listener keeps serving after one bad session.
                let setup = Setup {
                    args: &args,
                    book: &book,
                    window: &window,
                    me: &me,
                    platform: platform.clone(),
                };
                if let Err(e) = session(link, Direction::Inbound, setup).await {
                    log::warn(&e);
                }
            }
            Err(e) => println!("Incoming connection failed: {e}"),
        }
    }
    Ok(())
}

/// Parses arguments, arms the optional exit timer, and serves on the chosen platform.
#[tokio::main]
async fn main() -> ExitCode {
    log::init(&store_dir());
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            log::warn(&e);
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
    let result = if args.headless {
        let me = Me {
            name: args.name.clone(),
            os: Os::Linux,
            width: 1920,
            height: 1080,
            features: 0,
        };
        serve(args, Headless, me).await
    } else {
        start_native(args).await
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::warn(&e);
            ExitCode::FAILURE
        }
    }
}

/// Starts the real input adapter and serves with this device's display size.
#[cfg(any(target_os = "macos", windows, free_unix))]
async fn start_native(args: Args) -> Result<(), String> {
    // Before the first display query: Windows refuses the change once a window exists.
    #[cfg(windows)]
    if let Some(w) = overdesk_platform::win::ffi::ensure_dpi_aware() {
        log::warn(w);
    }
    let (width, height) = overdeskd::native::main_display_size();
    let os = this_os();
    let me = Me {
        name: args.name.clone(),
        os,
        width,
        height,
        features: 0,
    };
    let platform = overdeskd::native::NativePlatform::start()
        .map_err(|e| format!("could not start keyboard and mouse sharing: {e}"))?;
    serve(args, platform, me).await
}

/// Fails on operating systems without an input adapter.
#[cfg(not(any(target_os = "macos", windows, free_unix)))]
async fn start_native(_args: Args) -> Result<(), String> {
    Err("no input adapter for this OS yet; use --headless".into())
}
