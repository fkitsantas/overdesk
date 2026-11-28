// SPDX-License-Identifier: AGPL-3.0-only
//! Two daemon sessions over real loopback QUIC, each on a fake platform.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use overdesk_core::engine::Action;
use overdesk_net::pairing::Fingerprint;
use overdesk_net::session::Me;
use overdesk_net::transport::{CloseReason, Link};
use overdesk_net::transport::{Identity, Node};
use overdesk_platform::input::Captured;
use overdesk_proto::{ClipItem, ClipKind, Control, Os};
use overdeskd::testing::Recording;
use overdeskd::{Admit, AdmitFuture, Clipboard, DaemonError, Introduced, Platform, Side, establish, run};

/// Polls `cond` every 25 ms for up to five seconds; panics with `what` on timeout.
async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn mac_drives_pc_and_pc_cleans_up_when_mac_vanishes() {
    let (mac_id, pc_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (mac_fp, pc_fp) = (mac_id.fingerprint(), pc_id.fingerprint());
    let mac_node = Node::bind(&mac_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let pc_node = Node::bind(&pc_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = pc_node.local_addr().unwrap();
    let (mac_link, pc_link) = tokio::join!(mac_node.connect(addr), async { pc_node.accept().await.unwrap().await });

    let mac_me = Me {
        name: "mac".into(),
        os: Os::MacOs,
        width: 1440,
        height: 900,
        features: 0,
    };
    let pc_me = Me {
        name: "pc".into(),
        os: Os::Windows,
        width: 1920,
        height: 1080,
        features: 0,
    };
    let (mac, pc) = tokio::join!(
        establish(mac_link.unwrap(), |fp| *fp == pc_fp, &mac_me, Side::Right, None),
        establish(pc_link.unwrap(), |fp| *fp == mac_fp, &pc_me, Side::Right, None),
    );
    let (mac, pc) = (mac.unwrap(), pc.unwrap());
    assert_eq!((mac.peer_name.as_str(), pc.peer_name.as_str()), ("pc", "mac"));

    let (mac_fake, pc_fake) = (Recording::default(), Recording::default());
    let (mac_local, mac_capture) = tokio::sync::mpsc::unbounded_channel();
    let (_pc_local, pc_capture) = tokio::sync::mpsc::unbounded_channel();
    let mac_task = tokio::spawn(run(mac, mac_capture, mac_fake.clone(), |_| {}));
    let pc_task = tokio::spawn(run(pc, pc_capture, pc_fake.clone(), |_| {}));

    // Push past the Mac's right edge, then press Command.
    mac_local
        .send(Captured::Moved {
            dx: 5,
            dy: 0,
            x: 1439,
            y: 450,
        })
        .unwrap();
    mac_local
        .send(Captured::Key {
            code: 0xE3,
            pressed: true,
        })
        .unwrap();

    let pc_log = pc_fake.log.clone();
    wait_for("PC to inject the entry point", || {
        pc_log.lock().unwrap().contains(&Action::InjectMove { x: 4, y: 540 })
    })
    .await;
    wait_for("PC to inject Cmd as Ctrl", || {
        pc_log.lock().unwrap().contains(&Action::InjectKey {
            code: 0xE0,
            pressed: true,
        })
    })
    .await;
    assert!(
        *mac_fake.swallowing.lock().unwrap(),
        "Mac swallows while driving the PC"
    );

    // The Mac disappears without a goodbye: the PC must let go of everything.
    mac_task.abort();
    drop(mac_node);
    wait_for("PC to release all", || {
        pc_log.lock().unwrap().contains(&Action::ReleaseAll)
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), pc_task)
        .await
        .expect("PC session ends")
        .unwrap();
}

#[tokio::test]
async fn clipboard_text_follows_copies_without_echo() {
    let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (a_fp, b_fp) = (a_id.fingerprint(), b_id.fingerprint());
    let a_node = Node::bind(&a_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let b_node = Node::bind(&b_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = b_node.local_addr().unwrap();
    let (a_link, b_link) = tokio::join!(a_node.connect(addr), async { b_node.accept().await.unwrap().await });
    let me = |name: &str| Me {
        name: name.into(),
        os: Os::MacOs,
        width: 1440,
        height: 900,
        features: 0,
    };
    let (a_me, b_me) = (me("a"), me("b"));
    let (a, b) = tokio::join!(
        establish(a_link.unwrap(), |fp| *fp == b_fp, &a_me, Side::Right, None),
        establish(b_link.unwrap(), |fp| *fp == a_fp, &b_me, Side::Right, None),
    );
    let (a_fake, b_fake) = (Recording::default(), Recording::default());
    // Copied before the session: must stay private.
    *a_fake.clipboard.lock().unwrap() = Some("before".into());
    let (_a_local, a_capture) = tokio::sync::mpsc::unbounded_channel();
    let (_b_local, b_capture) = tokio::sync::mpsc::unbounded_channel();
    let _a = tokio::spawn(run(a.unwrap(), a_capture, a_fake.clone(), |_| {}));
    let _b = tokio::spawn(run(b.unwrap(), b_capture, b_fake.clone(), |_| {}));

    // Let both sessions take their baseline reading before the first in-session copy.
    tokio::time::sleep(Duration::from_millis(200)).await;
    *a_fake.clipboard.lock().unwrap() = Some("hello".into());
    let b_clip = b_fake.clipboard.clone();
    wait_for("B to receive the text", || {
        b_clip.lock().unwrap().as_deref() == Some("hello")
    })
    .await;

    // B changed locally after receiving; if A's text had echoed, this would be overwritten.
    *b_fake.clipboard.lock().unwrap() = Some("reply".into());
    let a_clip = a_fake.clipboard.clone();
    wait_for("A to receive the reply", || {
        a_clip.lock().unwrap().as_deref() == Some("reply")
    })
    .await;
    assert_ne!(b_fake.clipboard.lock().unwrap().as_deref(), Some("before"));
}

#[tokio::test]
async fn free_daemon_ignores_image_and_file_offers() {
    let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (a_fp, b_fp) = (a_id.fingerprint(), b_id.fingerprint());
    let a_node = Node::bind(&a_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let b_node = Node::bind(&b_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = b_node.local_addr().unwrap();
    let (a_link, b_link) = tokio::join!(a_node.connect(addr), async { b_node.accept().await.unwrap().await });
    let me = |name: &str| Me {
        name: name.into(),
        os: Os::MacOs,
        width: 1440,
        height: 900,
        features: 0,
    };
    let (a_me, b_me) = (me("a"), me("b"));
    let (a, b) = tokio::join!(
        establish(a_link.unwrap(), |fp| *fp == b_fp, &a_me, Side::Right, None),
        establish(b_link.unwrap(), |fp| *fp == a_fp, &b_me, Side::Right, None),
    );
    // Only B runs the daemon; A is a bare peer that sends what a Pro peer would.
    let mut a = a.unwrap();
    let b_fake = Recording::default();
    let (_b_local, b_capture) = tokio::sync::mpsc::unbounded_channel();
    let b_task = tokio::spawn(run(b.unwrap(), b_capture, b_fake.clone(), |_| {}));
    for msg in [
        Control::ClipOffer {
            id: [1; 16],
            kind: ClipKind::Files,
            items: vec![ClipItem {
                name: "a.txt".into(),
                size: 5,
            }],
        },
        Control::ClipGet { id: [1; 16] },
        Control::ClipCancel { id: [1; 16] },
    ] {
        a.tx.send_control(&msg).await.unwrap();
    }
    // The session still works afterwards: text arrives and the daemon is still running.
    a.tx.send_control(&Control::Clipboard {
        text: "still here".into(),
    })
    .await
    .unwrap();
    let b_clip = b_fake.clipboard.clone();
    wait_for("B to receive text after the offers", || {
        b_clip.lock().unwrap().as_deref() == Some("still here")
    })
    .await;
    assert!(!b_task.is_finished(), "the offers ended the session");
}

/// Sets up an established pair over loopback: `(a, b)` sessions.
async fn pair_sessions() -> (overdeskd::Session, overdeskd::Session) {
    let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (a_fp, b_fp) = (a_id.fingerprint(), b_id.fingerprint());
    let a_node = Node::bind(&a_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let b_node = Node::bind(&b_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = b_node.local_addr().unwrap();
    let (a_link, b_link) = tokio::join!(a_node.connect(addr), async { b_node.accept().await.unwrap().await });
    let me = |name: &str| Me {
        name: name.into(),
        os: Os::MacOs,
        width: 1440,
        height: 900,
        features: 0,
    };
    let (a_me, b_me) = (me("a"), me("b"));
    let (a, b) = tokio::join!(
        establish(a_link.unwrap(), |fp| *fp == b_fp, &a_me, Side::Right, None),
        establish(b_link.unwrap(), |fp| *fp == a_fp, &b_me, Side::Right, None),
    );
    (a.unwrap(), b.unwrap())
}

/// A platform whose clipboard writes are slow and which notes whether two ever overlapped.
#[derive(Clone, Default)]
struct SlowClip {
    busy: Arc<std::sync::atomic::AtomicBool>,
    overlapped: Arc<std::sync::atomic::AtomicBool>,
    written: Arc<Mutex<Vec<String>>>,
}

impl Clipboard for SlowClip {
    fn set_clipboard_text(&self, text: &str) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.busy.swap(true, SeqCst) {
            self.overlapped.store(true, SeqCst);
        }
        std::thread::sleep(Duration::from_millis(40));
        self.written.lock().unwrap().push(text.into());
        self.busy.store(false, SeqCst);
    }
}

impl Platform for SlowClip {
    fn swallow(&self, _on: bool) {}
    fn inject(&self, _action: Action) {}
}

#[tokio::test]
async fn clipboard_writes_never_overlap_and_end_on_the_newest_text() {
    let (mut a, b) = pair_sessions().await;
    let fake = SlowClip::default();
    let (_local, capture) = tokio::sync::mpsc::unbounded_channel();
    let _b = tokio::spawn(run(b, capture, fake.clone(), |_| {}));
    for i in 0..6 {
        a.tx.send_control(&Control::Clipboard { text: format!("t{i}") })
            .await
            .unwrap();
    }
    wait_for("the newest text to be written", || {
        fake.written.lock().unwrap().last().map(String::as_str) == Some("t5")
    })
    .await;
    assert!(
        !fake.overlapped.load(std::sync::atomic::Ordering::SeqCst),
        "one writer at a time"
    );
}

#[tokio::test]
async fn aborting_a_session_still_hands_input_back() {
    let (_a, b) = pair_sessions().await;
    let fake = Recording::default();
    *fake.swallowing.lock().unwrap() = true; // as if the cursor were on the peer when it dies
    let (_local, capture) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(run(b, capture, fake.clone(), |_| {}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    task.abort();
    let _ = task.await;
    assert!(!*fake.swallowing.lock().unwrap());
    assert!(fake.log.lock().unwrap().contains(&Action::ReleaseAll));
}

/// A dialer that completes `Hello`, then sends `desk_for(dialer, listener)` instead of an honest Desk;
/// returns what the listener's `establish` made of it.
async fn listener_gets(
    desk_for: impl FnOnce(overdesk_proto::PeerId, overdesk_proto::PeerId) -> Option<overdesk_proto::Desk>,
) -> Result<overdeskd::Session, overdeskd::DaemonError> {
    let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (a_fp, b_fp) = (a_id.fingerprint(), b_id.fingerprint());
    let a_node = Node::bind(&a_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let b_node = Node::bind(&b_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = b_node.local_addr().unwrap();
    let (a_link, b_link) = tokio::join!(a_node.connect(addr), async { b_node.accept().await.unwrap().await });
    let me = |name: &str| Me {
        name: name.into(),
        os: Os::MacOs,
        width: 1440,
        height: 900,
        features: 0,
    };
    let (a_me, b_me) = (me("a"), me("b"));
    let (ids, b) = (
        (
            overdesk_net::pairing::peer_id(&a_fp),
            overdesk_net::pairing::peer_id(&b_fp),
        ),
        establish(b_link.unwrap(), |fp| *fp == a_fp, &b_me, Side::Right, None),
    );
    let dialer = async {
        let mut link = a_link.unwrap();
        overdeskd::introduce(&mut link, |fp| *fp == b_fp, &a_me).await.unwrap();
        if let Some(desk) = desk_for(ids.0, ids.1) {
            link.send_control(&Control::Desk(desk)).await.unwrap();
        }
        link // kept open so the listener sees a stall, not a close
    };
    let (b, _link) = tokio::join!(b, dialer);
    b
}

/// A Desk of the two peers, side by side, with the given sizes.
fn desk_of(a: overdesk_proto::PeerId, b: overdesk_proto::PeerId, sizes: [(u32, u32); 2]) -> overdesk_proto::Desk {
    let screen = |peer, x, (w, h)| overdesk_proto::DeskScreen {
        peer,
        name: peer.to_hex(),
        x,
        y: 0,
        w,
        h,
    };
    overdesk_proto::Desk {
        version: 1,
        author: a,
        screens: vec![screen(a, 0, sizes[0]), screen(b, sizes[0].0 as i32, sizes[1])],
    }
}

#[tokio::test]
async fn a_desk_whose_sizes_differ_from_hello_is_refused() {
    let honest = listener_gets(|a, b| Some(desk_of(a, b, [(1440, 900), (1440, 900)]))).await;
    assert!(honest.is_ok());
    for sizes in [
        [(1441, 900), (1440, 900)],
        [(1440, 900), (1440, 901)],
        [(1440, 900), (0, 0)],
    ] {
        let r = listener_gets(|a, b| Some(desk_of(a, b, sizes))).await;
        assert!(
            matches!(
                r,
                Err(overdeskd::DaemonError::DeskMismatch | overdeskd::DaemonError::Desk(_))
            ),
            "{sizes:?}"
        );
    }
}

#[tokio::test]
async fn establish_gives_up_on_a_peer_that_never_sends_the_desk() {
    let started = std::time::Instant::now();
    let r = listener_gets(|_, _| None).await;
    assert!(matches!(
        r,
        Err(overdeskd::DaemonError::Session(
            overdesk_net::session::SessionError::TimedOut
        ))
    ));
    assert!(started.elapsed() < Duration::from_secs(15));
}

/// A hook that records whom it was asked about and answers as told.
struct Vet {
    seen: Mutex<Vec<(String, Fingerprint)>>,
    verdict: Result<(), CloseReason>,
}

impl Admit for Vet {
    fn admit<'a>(&'a self, _link: &'a mut Link, peer: &'a Introduced, fingerprint: &'a Fingerprint) -> AdmitFuture<'a> {
        self.seen.lock().unwrap().push((peer.name.clone(), *fingerprint));
        Box::pin(async move { self.verdict })
    }
}

/// Two linked, pinned ends: the hook runs on the first, which is the listener.
async fn admit_pair(
    vet: &Vet,
) -> (
    Result<overdeskd::Session, DaemonError>,
    Result<overdeskd::Session, DaemonError>,
    Fingerprint,
) {
    let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (a_fp, b_fp) = (a_id.fingerprint(), b_id.fingerprint());
    let a_node = Node::bind(&a_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let b_node = Node::bind(&b_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = a_node.local_addr().unwrap();
    let (b_link, a_link) = tokio::join!(b_node.connect(addr), async { a_node.accept().await.unwrap().await });
    let me = |name: &str| Me {
        name: name.into(),
        os: Os::Windows,
        width: 1920,
        height: 1080,
        features: 0,
    };
    let (me_a, me_b) = (me("a"), me("b"));
    let (a, b) = tokio::join!(
        establish(a_link.unwrap(), |fp| *fp == b_fp, &me_a, Side::Right, Some(vet)),
        establish(b_link.unwrap(), |fp| *fp == a_fp, &me_b, Side::Right, None),
    );
    (a, b, b_fp)
}

#[tokio::test]
async fn an_admit_hook_sees_the_peer_and_lets_it_in() {
    let vet = Vet {
        seen: Mutex::default(),
        verdict: Ok(()),
    };
    let (a, b, b_fp) = admit_pair(&vet).await;
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(*vet.seen.lock().unwrap(), vec![("b".to_owned(), b_fp)]);
}

#[tokio::test]
async fn an_admit_hook_that_refuses_closes_the_link_with_its_reason() {
    let vet = Vet {
        seen: Mutex::default(),
        verdict: Err(CloseReason::NeedsPro),
    };
    let (a, _dialer, _) = admit_pair(&vet).await;
    assert!(
        matches!(a, Err(DaemonError::Refused(CloseReason::NeedsPro))),
        "the typed reason"
    );
}

#[tokio::test]
async fn the_escape_chord_closes_a_link_whose_peer_stopped_reading() {
    let (mut a, _b_never_reads) = pair_sessions().await;
    // Fill the control stream's flow-control window so the next write cannot leave. Each
    // write is abandoned at its timeout, mid-frame, like the one `run` is about to drop.
    let big = Control::Clipboard {
        text: "x".repeat(1024 * 1024 - 64),
    };
    for _ in 0..8 {
        let _ = tokio::time::timeout(Duration::from_millis(200), a.tx.send_control(&big)).await;
    }
    let fake = Recording::default();
    let (local, capture) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(run(a, capture, fake.clone(), |_| {}));
    // Cross the right edge: the `Focus` message cannot leave, so the send stalls.
    local
        .send(Captured::Moved {
            dx: 5,
            dy: 0,
            x: 1439,
            y: 450,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "the stalled send is still pending");
    let start = std::time::Instant::now();
    local.send(Captured::Escape).unwrap();
    // Far below the five second send ceiling.
    let ended = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the session ends at once")
        .unwrap();
    assert_eq!(ended, overdeskd::Ended::Lost);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(!*fake.swallowing.lock().unwrap(), "input is handed back");
}
