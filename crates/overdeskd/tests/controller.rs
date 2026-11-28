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
use overdeskd::controller::trust::UnexpectedPeer;
use overdeskd::controller::{AppState, Command, Config, DeviceKind, PeerView, Phase, spawn};
use overdeskd::testing::Silent;
use overdeskd::{Clipboard, DaemonError, Platform, Side};
use tokio::sync::watch;

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
        admit: None,
        me: Me {
            name: name.into(),
            os,
            width: w,
            height: h,
            features: 0,
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
    let (a_cmd, mut a) = spawn(config("mac", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("pc", Os::Windows, 1920, 1080), Silent).await.unwrap();
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
            peers: vec![PeerView {
                id: sa.desk.iter().find(|d| !d.me).unwrap().id,
                name: "pc".into(),
                os: Os::Windows,
                features: overdeskd::native::input_features(), // both peers announce what this host has
                kind: DeviceKind::Computer,
                label: None,
            }],
            mode: Mode::Local
        }
    );
    assert!(matches!(sb.phase, Phase::Connected { ref peers, .. } if peers[0].name == "mac"));
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

    // Naming the connected peer ends the session like Disconnect; naming a stranger does nothing.
    let peer = sa.desk.iter().find(|d| !d.me).unwrap().id;
    a_cmd.send(Command::DisconnectPeer(PeerId([9; 16]))).unwrap();
    a_cmd.send(Command::DisconnectPeer(peer)).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    wait(&mut b, "B lost", |p| matches!(p, Phase::Lost { peer } if peer == "mac")).await;
}

#[tokio::test]
async fn android_tv_needs_pro() {
    let (a_cmd, mut a) = spawn(config("mac3", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("tv3", Os::Android, 1920, 1080), Silent).await.unwrap();
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
    b_cmd.send(Command::AnswerCode(true)).unwrap();
    let s = wait(&mut a, "A refused", |p| matches!(p, Phase::Error(_))).await;
    assert_eq!(s.phase, Phase::Error(DaemonError::NeedsPro.to_string()));
}

#[tokio::test]
async fn declined_pairing_returns_to_idle_with_a_reason() {
    let (a_cmd, mut a) = spawn(config("mac2", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("pc2", Os::Windows, 1920, 1080), Silent).await.unwrap();
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
    let (a_cmd, mut a) = spawn(config("mac3", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (_b_cmd, b) = spawn(config("pc3", Os::Windows, 1920, 1080), Silent).await.unwrap();
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
    let (a_cmd, mut a) = spawn(config("mac4", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("pc4", Os::Windows, 1920, 1080), Silent).await.unwrap();
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

impl Clipboard for Slow {}

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
    let (a_cmd, mut a) = spawn(config("exp-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, b) = spawn(config("exp-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
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
    assert_eq!(got.phase, Phase::Error(UnexpectedPeer.to_string()));
    for _ in 0..20 {
        assert!(!matches!(a.borrow().phase, Phase::Pairing { .. }), "A never prompts");
        assert!(!matches!(b.borrow().phase, Phase::Pairing { .. }), "B never prompts");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn expecting_the_pinned_device_reconnects() {
    let (a_cmd, mut a) = spawn(config("rec-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("rec-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
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
async fn forgetting_a_computer_ends_the_session_and_removes_its_pin() {
    let (a_cmd, mut a) = spawn(config("fg-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("fg-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    let dir = std::env::temp_dir().join(format!("overdesk-ctl-fg-a-{}", std::process::id()));
    assert_eq!(Store::open(&dir).unwrap().pins().unwrap().len(), 1);
    // A stranger is a stale click: the session carries on and the pin stays.
    a_cmd.send(Command::Forget(PeerId([9; 16]))).unwrap();
    a_cmd.send(Command::Forget(id_of("fg-b"))).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    // B is told it was forgotten: it unpins A and says so, instead of showing a lost link.
    let sb = wait(&mut b, "B idle", |p| *p == Phase::Idle).await;
    assert_eq!(
        sb.notice.as_deref(),
        Some("fg-a forgot this computer. Pair again to reconnect.")
    );
    assert!(sb.paired.is_empty(), "B no longer lists A as paired");
    let b_dir = std::env::temp_dir().join(format!("overdesk-ctl-fg-b-{}", std::process::id()));
    assert!(Store::open(&b_dir).unwrap().pins().unwrap().is_empty());
    assert!(Store::open(&dir).unwrap().pins().unwrap().is_empty());
    let nearby_paired = a.borrow().nearby.iter().any(|d| d.paired);
    assert!(!nearby_paired);
}

#[tokio::test]
async fn forgetting_a_computer_mid_dial_aborts_the_attempt() {
    let (a_cmd, mut a) = spawn(config("fd-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("fd-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    a_cmd.send(Command::Disconnect).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    let addr = b.borrow().address.unwrap();
    a_cmd
        .send(Command::Connect {
            address: addr,
            side: Side::Right,
            expect: Some(id_of("fd-b")),
        })
        .unwrap();
    a_cmd.send(Command::Forget(id_of("fd-b"))).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    for _ in 0..40 {
        let phase = a.borrow().phase.clone();
        assert_eq!(phase, Phase::Idle, "a forgotten device never connects or errors");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_forgotten_computer_dialing_in_is_refused() {
    let (a_cmd, mut a) = spawn(config("fi-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("fi-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    b_cmd.send(Command::Forget(id_of("fi-a"))).unwrap();
    wait(&mut b, "B idle", |p| *p == Phase::Idle).await;
    a_cmd.send(Command::Disconnect).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    let addr = b.borrow().address.unwrap();
    a_cmd
        .send(Command::Connect {
            address: addr,
            side: Side::Right,
            expect: None,
        })
        .unwrap();
    for _ in 0..40 {
        assert!(!matches!(
            b.borrow().phase,
            Phase::Connected { .. } | Phase::Pairing { .. }
        ));
        assert!(!matches!(a.borrow().phase, Phase::Connected { .. }));
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn probes_and_half_open_dials_leave_the_target_idle() {
    let (_cmd, mut t) = spawn(config("probe-target", Os::Linux, 1280, 720), Silent)
        .await
        .unwrap();
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
    let (b_cmd, mut b) = spawn(config("gen-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
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
    let (a_cmd, mut a) = spawn(config("win-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("win-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
    a_cmd.send(Command::AllowPairing(true)).unwrap();
    a.wait_for(|s| s.pairing_open).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    assert!(a.borrow().pairing_open, "A dialed, so its own window stays open");
    assert!(!b.borrow().pairing_open, "B paired inbound, so its window closes");
}

#[tokio::test]
async fn connect_while_connected_leaves_the_session_alone() {
    let (a_cmd, mut a) = spawn(config("busy-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("busy-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
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
        admit: None,
        ..config(name, os, 1440, 900)
    };
    let (_a_cmd, mut a) = spawn(on("disc-a", Os::MacOs), Silent).await.unwrap();
    let (_b_cmd, mut b) = spawn(on("disc-b", Os::Windows), Silent).await.unwrap();
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

/// A platform whose session panics as soon as it starts, and which records the cleanup.
#[derive(Clone, Default)]
struct Panicky {
    released: Arc<AtomicBool>,
}

impl Clipboard for Panicky {}

impl Platform for Panicky {
    fn swallow(&self, _on: bool) {}
    fn inject(&self, action: Action) {
        if action == Action::ReleaseAll {
            self.released.store(true, Ordering::SeqCst);
        }
    }
    fn monitors(&self) -> Vec<overdesk_core::Rect> {
        panic!("simulated bug in the session");
    }
}

#[tokio::test]
async fn a_panicking_session_shows_an_error_and_hands_input_back() {
    let p = Panicky::default();
    let (a_cmd, mut a) = spawn(config("pan-a", Os::MacOs, 1440, 900), p.clone()).await.unwrap();
    let (b_cmd, mut b) = spawn(config("pan-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
    let b_addr = b.borrow().address.unwrap();
    b_cmd.send(Command::AllowPairing(true)).unwrap();
    a_cmd
        .send(Command::Connect {
            address: b_addr,
            side: Side::Right,
            expect: None,
        })
        .unwrap();
    wait(&mut a, "A code", |p| matches!(p, Phase::Pairing { .. })).await;
    wait(&mut b, "B code", |p| matches!(p, Phase::Pairing { .. })).await;
    a_cmd.send(Command::AnswerCode(true)).unwrap();
    b_cmd.send(Command::AnswerCode(true)).unwrap();
    wait(&mut a, "A error", |p| matches!(p, Phase::Error(_))).await;
    assert!(p.released.load(Ordering::SeqCst), "held input was released");
}

#[tokio::test]
async fn an_answer_to_an_old_code_is_ignored() {
    let (a_cmd, mut a) = spawn(config("stale-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("stale-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
    let b_addr = b.borrow().address.unwrap();
    b_cmd.send(Command::AllowPairing(true)).unwrap();
    a_cmd
        .send(Command::Connect {
            address: b_addr,
            side: Side::Right,
            expect: None,
        })
        .unwrap();
    let Phase::Pairing { code, .. } = wait(&mut a, "A code", |p| matches!(p, Phase::Pairing { .. }))
        .await
        .phase
    else {
        unreachable!()
    };
    wait(&mut b, "B code", |p| matches!(p, Phase::Pairing { .. })).await;
    // A click that belonged to another prompt does nothing.
    a_cmd
        .send(Command::Answer {
            code: (code + 1) % 1_000_000,
            matches: false,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        matches!(a.borrow().phase, Phase::Pairing { .. }),
        "{:?}",
        a.borrow().phase
    );
    a_cmd.send(Command::Answer { code, matches: true }).unwrap();
    b_cmd.send(Command::AnswerCode(true)).unwrap();
    wait(&mut a, "A connected", |p| matches!(p, Phase::Connected { .. })).await;
}

#[tokio::test]
async fn dismissing_an_error_does_not_switch_off_reconnecting() {
    // Dialing a port nobody listens on ends on an error screen; Disconnect dismisses it.
    let (a_cmd, mut a) = spawn(config("dis-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    a_cmd
        .send(Command::Connect {
            address: "127.0.0.1:9".parse().unwrap(),
            side: Side::Right,
            expect: None,
        })
        .unwrap();
    wait(&mut a, "A error", |p| matches!(p, Phase::Error(_))).await;
    a_cmd.send(Command::Disconnect).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
}

#[tokio::test]
async fn a_paired_device_that_is_not_nearby_is_listed_and_can_be_forgotten() {
    let cfg = config("pl-a", Os::MacOs, 1440, 900);
    // Paired in an earlier run; nothing is on the network (discovery is off).
    let fp = [7u8; 32];
    Store::open(&cfg.store_dir).unwrap().pin(fp).unwrap();
    let id = peer_id(&fp);
    let (cmd, a) = spawn(cfg, Silent).await.unwrap();
    let listed = a.borrow().paired.clone();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].id, listed[0].kind, listed[0].online),
        (id, DeviceKind::Computer, false)
    );
    assert_eq!(listed[0].name.len(), 8, "named by the start of its id until heard");
    drop(cmd);
    // A saved name (control characters stripped on the way in) names it at the next start.
    let dir = std::env::temp_dir().join(format!("overdesk-ctl-pl-a-{}", std::process::id()));
    std::fs::write(
        dir.join("names.txt"),
        format!("{}\tOffice\u{7} PC\nnot a line\n", id.to_hex()),
    )
    .unwrap();
    let (cmd, mut a) = spawn(again("pl-a"), Silent).await.unwrap();
    assert_eq!(a.borrow().paired[0].name, "Office PC");
    cmd.send(Command::Forget(id)).unwrap();
    tokio::time::timeout(Duration::from_secs(10), a.wait_for(|s| s.paired.is_empty()))
        .await
        .expect("forgotten in time")
        .unwrap();
    let saved = std::fs::read_to_string(dir.join("names.txt")).unwrap();
    assert!(!saved.contains("Office"), "its name is deleted with it: {saved:?}");
}

/// A configuration for a device whose store folder already exists (a restart).
fn again(name: &str) -> Config {
    Config {
        store_dir: std::env::temp_dir().join(format!("overdesk-ctl-{name}-{}", std::process::id())),
        listen: "127.0.0.1:0".parse().unwrap(),
        discovery: false,
        admit: None,
        me: Me {
            name: name.into(),
            os: Os::MacOs,
            width: 1440,
            height: 900,
            features: 0,
        },
    }
}

#[tokio::test]
async fn a_paired_computers_name_is_saved_and_survives_a_restart() {
    let (a_cmd, mut a) = spawn(config("nm-a", Os::MacOs, 1440, 900), Silent).await.unwrap();
    let (b_cmd, mut b) = spawn(config("nm-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    drop(a_cmd);
    let (_cmd, a) = spawn(again("nm-a"), Silent).await.unwrap();
    let listed = a.borrow().paired.clone();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].id, listed[0].name.as_str()), (id_of("nm-b"), "nm-b"));
}

/// A platform whose display size can be changed between sessions.
#[derive(Clone)]
struct Resizable(Arc<std::sync::atomic::AtomicU32>);

impl Clipboard for Resizable {}

impl Platform for Resizable {
    fn swallow(&self, _on: bool) {}
    fn inject(&self, _action: Action) {}
    fn display_size(&self) -> Option<(u32, u32)> {
        Some((self.0.load(Ordering::SeqCst), 900))
    }
}

#[tokio::test]
async fn a_display_change_reaches_the_peer_at_the_next_session() {
    let width = Arc::new(std::sync::atomic::AtomicU32::new(1440));
    let (a_cmd, mut a) = spawn(config("rs-a", Os::MacOs, 1, 1), Resizable(width.clone()))
        .await
        .unwrap();
    let (b_cmd, mut b) = spawn(config("rs-b", Os::Windows, 1920, 1080), Silent).await.unwrap();
    connect_pair(&a_cmd, &mut a, &b_cmd, &mut b).await;
    let mine = |s: &AppState| s.desk.iter().find(|d| d.me).map(|d| d.w);
    assert_eq!(mine(&a.borrow()), Some(1440), "not the 1 pixel it started with");
    a_cmd.send(Command::Disconnect).unwrap();
    wait(&mut a, "A idle", |p| *p == Phase::Idle).await;
    wait(&mut b, "B lost", |p| matches!(p, Phase::Lost { .. })).await;
    width.store(1000, Ordering::SeqCst);
    a_cmd
        .send(Command::Connect {
            address: b.borrow().address.unwrap(),
            side: Side::Right,
            expect: Some(id_of("rs-b")),
        })
        .unwrap();
    wait(&mut a, "A reconnected", |p| matches!(p, Phase::Connected { .. })).await;
    assert_eq!(mine(&a.borrow()), Some(1000));
    let wb = wait(&mut b, "B reconnected", |p| matches!(p, Phase::Connected { .. })).await;
    assert_eq!(
        wb.desk.iter().find(|d| !d.me).map(|d| d.w),
        Some(1000),
        "B sees the new size"
    );
}

#[tokio::test]
async fn a_command_that_changes_nothing_publishes_nothing() {
    let (cmd, mut rx) = spawn(config("quiet", Os::Linux, 800, 600), Silent).await.unwrap();
    rx.mark_unchanged();
    // No notice to dismiss and pairing already closed: neither may wake the UI.
    cmd.send(Command::DismissNotice).unwrap();
    cmd.send(Command::AllowPairing(false)).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!rx.has_changed().unwrap(), "nothing changed, nothing published");
    // A real change still comes through.
    cmd.send(Command::AllowPairing(true)).unwrap();
    rx.changed().await.unwrap();
    assert!(rx.borrow_and_update().pairing_open);
}
