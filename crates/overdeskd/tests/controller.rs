// SPDX-License-Identifier: AGPL-3.0-only
//! Two controllers (what the app runs) over real loopback QUIC, on fake platforms.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use overdesk_core::engine::{Action, Mode};
use overdesk_net::pairing::peer_id;
use overdesk_net::session::Me;
use overdesk_net::store::Store;
use overdesk_net::transport::{Identity, Node};
use overdesk_proto::{Os, PeerId};
use overdeskd::controller::{AppState, Command, Config, Phase, spawn};
use overdeskd::{Platform, Side};
use tokio::sync::watch;

/// A platform that ignores every action, for tests that only watch controller state.
#[derive(Clone)]
struct Fake;

impl Platform for Fake {
    fn swallow(&self, _on: bool) {}
    fn inject(&self, _action: Action) {}
}

/// A fresh, empty store folder for one controller.
fn store(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("overdesk-ctl-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A loopback controller configuration for a device with the given name, OS and screen.
fn config(name: &str, os: Os, w: u32, h: u32) -> Config {
    Config {
        store_dir: store(name),
        listen: "127.0.0.1:0".parse().unwrap(),
        discovery: false,
        me: Me {
            name: name.into(),
            os,
            width: w,
            height: h,
        },
    }
}

/// Waits up to ten seconds for the controller's phase to satisfy `ok`, then returns
/// that state; panics with `what` on timeout or if the controller stops.
async fn wait(rx: &mut watch::Receiver<AppState>, what: &str, ok: impl Fn(&Phase) -> bool) -> AppState {
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        rx.wait_for(|s| ok(&s.phase)).await.map(|s| s.clone())
    })
    .await;
    match got {
        Ok(Ok(state)) => state,
        Ok(Err(_)) => panic!("controller stopped while waiting for {what}"),
        Err(_) => panic!("timed out waiting for {what}; last: {:?}", rx.borrow().phase),
    }
}

#[tokio::test]
async fn pair_connect_and_lose_a_peer() {
    let (a_cmd, mut a) = spawn(config("mac", Os::MacOs, 1440, 900), Fake).await.unwrap();
    let (b_cmd, mut b) = spawn(config("pc", Os::Windows, 1920, 1080), Fake).await.unwrap();
    let b_addr = b.borrow().address.expect("B is listening");
    b_cmd.send(Command::AllowPairing(true)).unwrap();
    wait(&mut b, "B pairing armed", |_| true).await;

    a_cmd
        .send(Command::Connect {
            address: b_addr,
            side: Side::Right,
            expect: None,
        })
        .unwrap();
    let pa = wait(&mut a, "A pairing code", |p| matches!(p, Phase::Pairing { .. })).await;
    let pb = wait(&mut b, "B pairing code", |p| matches!(p, Phase::Pairing { .. })).await;
    let (Phase::Pairing { code: ca, .. }, Phase::Pairing { code: cb, .. }) = (pa.phase, pb.phase) else {
        unreachable!()
    };
    assert_eq!(ca, cb, "both screens show the same code");

    a_cmd.send(Command::AnswerCode(true)).unwrap();
    b_cmd.send(Command::AnswerCode(true)).unwrap();
    let sa = wait(&mut a, "A connected", |p| matches!(p, Phase::Connected { .. })).await;
    let sb = wait(&mut b, "B connected", |p| matches!(p, Phase::Connected { .. })).await;
    assert_eq!(
        sa.phase,
        Phase::Connected {
            peer: "pc".into(),
            peer_os: Os::Windows,
            mode: Mode::Local
        }
    );
    assert!(matches!(sb.phase, Phase::Connected { ref peer, .. } if peer == "mac"));
    let names: Vec<_> = sa.desk.iter().map(|s| (s.name.as_str(), s.me, s.x)).collect();
    assert_eq!(
        names,
        vec![("mac", true, 0), ("pc", false, 1440)],
        "Desk uses device names, PC on the right"
    );

    // The dialer remembers where the peer's screen sits, for automatic reconnects.
    // (`store()` would wipe the folder, so rebuild its path by hand.)
    let dir = std::env::temp_dir().join(format!("overdesk-ctl-mac-{}", std::process::id()));
    let sides = std::fs::read_to_string(dir.join("sides.txt")).unwrap();
    assert!(sides.trim_end().ends_with(" right"), "saved side: {sides:?}");

    a_cmd.send(Command::Disconnect).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    wait(&mut b, "B lost", |p| matches!(p, Phase::Lost { peer } if peer == "mac")).await;
}

#[tokio::test]
async fn declined_pairing_returns_to_idle_with_a_reason() {
    let (a_cmd, mut a) = spawn(config("mac2", Os::MacOs, 1440, 900), Fake).await.unwrap();
    let (b_cmd, mut b) = spawn(config("pc2", Os::Windows, 1920, 1080), Fake).await.unwrap();
    let b_addr = b.borrow().address.unwrap();
    b_cmd.send(Command::AllowPairing(true)).unwrap();
    a_cmd
        .send(Command::Connect {
            address: b_addr,
            side: Side::Right,
            expect: None,
        })
        .unwrap();
    wait(&mut a, "A pairing", |p| matches!(p, Phase::Pairing { .. })).await;
    wait(&mut b, "B pairing", |p| matches!(p, Phase::Pairing { .. })).await;
    a_cmd.send(Command::AnswerCode(true)).unwrap();
    b_cmd.send(Command::AnswerCode(false)).unwrap();
    wait(&mut a, "A error", |p| matches!(p, Phase::Error(_))).await;
    wait(&mut b, "B back to idle", |p| matches!(p, Phase::Idle | Phase::Error(_))).await;
}

#[tokio::test]
async fn unknown_device_cannot_prompt_unless_pairing_is_open() {
    let (a_cmd, mut a) = spawn(config("mac3", Os::MacOs, 1440, 900), Fake).await.unwrap();
    let (_b_cmd, b) = spawn(config("pc3", Os::Windows, 1920, 1080), Fake).await.unwrap();
    let b_addr = b.borrow().address.unwrap();
    assert!(!b.borrow().pairing_open, "closed by default");
    a_cmd
        .send(Command::Connect {
            address: b_addr,
            side: Side::Right,
            expect: None,
        })
        .unwrap();
    wait(&mut a, "A refused", |p| matches!(p, Phase::Error(_))).await;
    assert_eq!(b.borrow().phase, Phase::Idle, "B never showed a code");
}

#[tokio::test]
async fn repeated_failed_pairings_back_off_even_with_the_window_open() {
    let (a_cmd, mut a) = spawn(config("mac4", Os::MacOs, 1440, 900), Fake).await.unwrap();
    let (b_cmd, mut b) = spawn(config("pc4", Os::Windows, 1920, 1080), Fake).await.unwrap();
    let b_addr = b.borrow().address.unwrap();
    b_cmd.send(Command::AllowPairing(true)).unwrap();
    let dial = || Command::Connect {
        address: b_addr,
        side: Side::Right,
        expect: None,
    };
    for _ in 0..3 {
        a_cmd.send(dial()).unwrap();
        wait(&mut a, "A code", |p| matches!(p, Phase::Pairing { .. })).await;
        wait(&mut b, "B code", |p| matches!(p, Phase::Pairing { .. })).await;
        a_cmd.send(Command::AnswerCode(true)).unwrap();
        b_cmd.send(Command::AnswerCode(false)).unwrap();
        wait(&mut a, "A error", |p| matches!(p, Phase::Error(_))).await;
        wait(&mut b, "B error", |p| matches!(p, Phase::Error(_))).await;
    }
    // The window is still open, but the fourth attempt is refused without a prompt.
    assert!(b.borrow().pairing_open);
    a_cmd.send(dial()).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(matches!(b.borrow().phase, Phase::Error(_)), "B never showed a code");
}

/// A platform whose first `swallow(false)` blocks its thread, standing in for a session
/// that is mid-cleanup (and so cannot be aborted) when the user disconnects.
#[derive(Clone)]
struct Slow {
    /// Set once the blocking call has been taken by whichever thread got there first.
    taken: Arc<AtomicBool>,
    /// Set while that call is blocked.
    stalled: Arc<AtomicBool>,
}

impl Platform for Slow {
    fn swallow(&self, on: bool) {
        if !on && !self.taken.swap(true, Ordering::SeqCst) {
            self.stalled.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(700));
        }
    }
    fn inject(&self, _action: Action) {}
}

/// Pairs `a` (dialing) with `b` (listening) and waits until both are connected.
async fn connect_pair(
    a_cmd: &tokio::sync::mpsc::UnboundedSender<Command>,
    a: &mut watch::Receiver<AppState>,
    b_cmd: &tokio::sync::mpsc::UnboundedSender<Command>,
    b: &mut watch::Receiver<AppState>,
) {
    let b_addr = b.borrow().address.expect("B is listening");
    b_cmd.send(Command::AllowPairing(true)).unwrap();
    a_cmd
        .send(Command::Connect {
            address: b_addr,
            side: Side::Right,
            expect: None,
        })
        .unwrap();
    wait(a, "A pairing", |p| matches!(p, Phase::Pairing { .. })).await;
    wait(b, "B pairing", |p| matches!(p, Phase::Pairing { .. })).await;
    a_cmd.send(Command::AnswerCode(true)).unwrap();
    b_cmd.send(Command::AnswerCode(true)).unwrap();
    wait(a, "A connected", |p| matches!(p, Phase::Connected { .. })).await;
    wait(b, "B connected", |p| matches!(p, Phase::Connected { .. })).await;
}

/// The id a device announces and is pinned under, read from its store folder.
fn id_of(name: &str) -> PeerId {
    let dir = std::env::temp_dir().join(format!("overdesk-ctl-{name}-{}", std::process::id()));
    peer_id(&Store::open(&dir).unwrap().identity().unwrap().fingerprint())
}

#[tokio::test]
async fn expecting_a_different_device_errors_and_never_pairs() {
    let (a_cmd, mut a) = spawn(config("exp-a", Os::MacOs, 1440, 900), Fake).await.unwrap();
    let (b_cmd, b) = spawn(config("exp-b", Os::Windows, 1920, 1080), Fake).await.unwrap();
    let b_addr = b.borrow().address.unwrap();
    // Pairing is open on B, so the old behavior would have fallen back to a code prompt.
    b_cmd.send(Command::AllowPairing(true)).unwrap();
    a_cmd
        .send(Command::Connect {
            address: b_addr,
            side: Side::Right,
            expect: Some(PeerId([7; 16])),
        })
        .unwrap();
    let got = wait(&mut a, "A error", |p| matches!(p, Phase::Error(_))).await;
    assert_eq!(
        got.phase,
        Phase::Error("This isn't the computer you paired with. Pair it again if it was reset.".into())
    );
    for _ in 0..20 {
        assert!(!matches!(a.borrow().phase, Phase::Pairing { .. }), "A never prompts");
        assert!(!matches!(b.borrow().phase, Phase::Pairing { .. }), "B never prompts");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn expecting_the_pinned_device_reconnects() {
    let (a_cmd, mut a) = spawn(config("rec-a", Os::MacOs, 1440, 900), Fake).await.unwrap();
    let (b_cmd, mut b) = spawn(config("rec-b", Os::Windows, 1920, 1080), Fake).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    a_cmd.send(Command::Disconnect).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    wait(&mut b, "B lost", |p| matches!(p, Phase::Lost { .. })).await;
    a_cmd
        .send(Command::Connect {
            address: b.borrow().address.unwrap(),
            side: Side::Right,
            expect: Some(id_of("rec-b")),
        })
        .unwrap();
    wait(&mut a, "A reconnected", |p| matches!(p, Phase::Connected { .. })).await;
}

#[tokio::test]
async fn probes_and_half_open_dials_leave_the_target_idle() {
    let (_cmd, mut t) = spawn(config("probe-target", Os::Linux, 1280, 720), Fake).await.unwrap();
    let addr: SocketAddr = t.borrow().address.unwrap();
    // Raw garbage on the listening port.
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.send_to(&[0xC0; 1200], addr).unwrap();
    // Dials that are cut off at every stage of setup.
    let stranger = Arc::new(Node::bind(&Identity::generate().unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap());
    for ms in 0..25u64 {
        let (n, h) = (
            stranger.clone(),
            tokio::spawn({
                let n = stranger.clone();
                async move {
                    let _ = n.connect(addr).await;
                }
            }),
        );
        tokio::time::sleep(Duration::from_millis(ms)).await;
        h.abort();
        drop(n);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(t.borrow().phase, Phase::Idle, "inbound setup failures stay silent");
    let _ = wait(&mut t, "still idle", |p| *p == Phase::Idle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dying_session_cannot_overwrite_idle_after_disconnect() {
    let slow = Slow {
        taken: Arc::new(AtomicBool::new(false)),
        stalled: Arc::new(AtomicBool::new(false)),
    };
    let (a_cmd, mut a) = spawn(config("gen-a", Os::MacOs, 1440, 900), slow.clone())
        .await
        .unwrap();
    let (b_cmd, mut b) = spawn(config("gen-b", Os::Windows, 1920, 1080), Fake).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;

    // B leaves; A's session starts its (blocking) cleanup and can no longer be aborted.
    b_cmd.send(Command::Disconnect).unwrap();
    let began = tokio::time::Instant::now();
    while !slow.stalled.load(Ordering::SeqCst) {
        assert!(began.elapsed() < Duration::from_secs(10), "A's session never ended");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    a_cmd.send(Command::Disconnect).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(a.borrow().phase, Phase::Idle, "the old session's end is ignored");
}

#[tokio::test]
async fn dialing_out_keeps_the_users_pairing_window_open() {
    let (a_cmd, mut a) = spawn(config("win-a", Os::MacOs, 1440, 900), Fake).await.unwrap();
    let (b_cmd, mut b) = spawn(config("win-b", Os::Windows, 1920, 1080), Fake).await.unwrap();
    a_cmd.send(Command::AllowPairing(true)).unwrap();
    a.wait_for(|s| s.pairing_open).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    assert!(a.borrow().pairing_open, "A dialed, so its own window stays open");
    assert!(!b.borrow().pairing_open, "B paired inbound, so its window closes");
}

#[tokio::test]
async fn connect_while_connected_leaves_the_session_alone() {
    let (a_cmd, mut a) = spawn(config("busy-a", Os::MacOs, 1440, 900), Fake).await.unwrap();
    let (b_cmd, mut b) = spawn(config("busy-b", Os::Windows, 1920, 1080), Fake).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    let addr = b.borrow().address.unwrap();
    a_cmd
        .send(Command::Connect {
            address: addr,
            side: Side::Left,
            expect: None,
        })
        .unwrap();
    // The live session's screen must survive a stray Connect.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        matches!(a.borrow().phase, Phase::Connected { .. }),
        "{:?}",
        a.borrow().phase
    );
}

#[tokio::test]
#[ignore = "needs multicast"]
async fn controllers_with_discovery_see_each_other_nearby() {
    let on = |name, os| Config {
        discovery: true,
        ..config(name, os, 1440, 900)
    };
    let (_a_cmd, mut a) = spawn(on("disc-a", Os::MacOs), Fake).await.unwrap();
    let (_b_cmd, mut b) = spawn(on("disc-b", Os::Windows), Fake).await.unwrap();
    let b_port = b.borrow().address.unwrap().port();
    let seen = tokio::time::timeout(Duration::from_secs(15), async {
        a.wait_for(|s| s.nearby.iter().any(|d| d.name == "disc-b"))
            .await
            .map(|s| s.nearby.clone())
    })
    .await
    .expect("A sees B within 15 s")
    .unwrap();
    let d = seen.iter().find(|d| d.name == "disc-b").unwrap();
    assert_eq!((d.os, d.paired, d.address.port()), (Os::Windows, false, b_port));
    assert!(!seen.iter().any(|d| d.name == "disc-a"), "never lists itself");
    tokio::time::timeout(
        Duration::from_secs(15),
        b.wait_for(|s| s.nearby.iter().any(|d| d.name == "disc-a")),
    )
    .await
    .expect("B sees A within 15 s")
    .unwrap();
}
