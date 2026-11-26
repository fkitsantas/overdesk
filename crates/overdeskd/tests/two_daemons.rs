// SPDX-License-Identifier: AGPL-3.0-only
//! Two daemon sessions over real loopback QUIC, each on a fake platform.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use overdesk_core::engine::Action;
use overdesk_net::session::Me;
use overdesk_net::transport::{Identity, Node};
use overdesk_platform::input::Captured;
use overdesk_proto::{ClipItem, ClipKind, Control, Os};
use overdeskd::{Platform, Side, establish, run};

/// A platform that records every injected action and the swallow flag, so tests can
/// assert what a session did.
#[derive(Clone, Default)]
struct Fake {
    log: Arc<Mutex<Vec<Action>>>,
    swallowing: Arc<Mutex<bool>>,
    clipboard: Arc<Mutex<Option<String>>>,
}

impl Platform for Fake {
    fn swallow(&self, on: bool) {
        *self.swallowing.lock().unwrap() = on;
    }
    fn inject(&self, action: Action) {
        self.log.lock().unwrap().push(action);
    }
    fn clipboard_text(&self) -> Option<String> {
        self.clipboard.lock().unwrap().clone()
    }
    fn set_clipboard_text(&self, text: &str) {
        *self.clipboard.lock().unwrap() = Some(text.into());
    }
}

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
    };
    let pc_me = Me {
        name: "pc".into(),
        os: Os::Windows,
        width: 1920,
        height: 1080,
    };
    let (mac, pc) = tokio::join!(
        establish(mac_link.unwrap(), |fp| *fp == pc_fp, &mac_me, Side::Right),
        establish(pc_link.unwrap(), |fp| *fp == mac_fp, &pc_me, Side::Right),
    );
    let (mac, pc) = (mac.unwrap(), pc.unwrap());
    assert_eq!((mac.peer_name.as_str(), pc.peer_name.as_str()), ("pc", "mac"));

    let (mac_fake, pc_fake) = (Fake::default(), Fake::default());
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
    };
    let (a_me, b_me) = (me("a"), me("b"));
    let (a, b) = tokio::join!(
        establish(a_link.unwrap(), |fp| *fp == b_fp, &a_me, Side::Right),
        establish(b_link.unwrap(), |fp| *fp == a_fp, &b_me, Side::Right),
    );
    let (a_fake, b_fake) = (Fake::default(), Fake::default());
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
    };
    let (a_me, b_me) = (me("a"), me("b"));
    let (a, b) = tokio::join!(
        establish(a_link.unwrap(), |fp| *fp == b_fp, &a_me, Side::Right),
        establish(b_link.unwrap(), |fp| *fp == a_fp, &b_me, Side::Right),
    );
    // Only B runs the daemon; A is a bare peer that sends what a Pro peer would.
    let mut a = a.unwrap();
    let b_fake = Fake::default();
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
