// SPDX-License-Identifier: AGPL-3.0-only
//! Two daemon sessions over real loopback QUIC, each on a fake platform.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use overdesk_core::engine::Action;
use overdesk_net::session::Me;
use overdesk_net::transport::{Identity, Node};
use overdesk_platform::input::Captured;
use overdesk_proto::Os;
use overdeskd::{Platform, Side, establish, run};

/// A platform that records every injected action and the swallow flag, so tests can
/// assert what a session did.
#[derive(Clone, Default)]
struct Fake {
    log: Arc<Mutex<Vec<Action>>>,
    swallowing: Arc<Mutex<bool>>,
}

impl Platform for Fake {
    fn swallow(&self, on: bool) {
        *self.swallowing.lock().unwrap() = on;
    }
    fn inject(&self, action: Action) {
        self.log.lock().unwrap().push(action);
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
