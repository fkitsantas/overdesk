// SPDX-License-Identifier: AGPL-3.0-only
//! Two real QUIC peers over 127.0.0.1.

use std::future::Future;
use std::time::Duration;

use overdesk_net::pairing::peer_id;
use overdesk_net::session::{Me, Role, SessionError, hello, pair};
use overdesk_net::transport::{Identity, Link, NetError, Node};
use overdesk_proto::{Control, MAX_FRAME_LEN, Motion, Os, PROTOCOL_VERSION, Pairing};

/// Every test is bounded: a hang is a failure, not a stuck CI job.
async fn within<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out")
}

struct Pair {
    dialer: Link,
    listener: Link,
    dialer_id: Identity,
    listener_id: Identity,
    _nodes: (Node, Node),
}

async fn connected() -> Pair {
    let (dialer_id, listener_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let a = Node::bind(&dialer_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let b = Node::bind(&listener_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = b.local_addr().unwrap();
    let (dialer, listener) = tokio::join!(a.connect(addr), b.accept());
    Pair {
        dialer: dialer.unwrap(),
        listener: listener.unwrap().unwrap(),
        dialer_id,
        listener_id,
        _nodes: (a, b),
    }
}

fn me(name: &str) -> Me {
    Me {
        name: name.into(),
        os: Os::MacOs,
        width: 1920,
        height: 1080,
    }
}

#[tokio::test]
async fn both_sides_see_the_other_certificate() {
    within(async {
        let p = connected().await;
        assert_eq!(p.dialer.peer_fingerprint(), p.listener_id.fingerprint());
        assert_eq!(p.listener.peer_fingerprint(), p.dialer_id.fingerprint());
    })
    .await;
}

#[tokio::test]
async fn control_frames_flow_both_ways_in_order() {
    within(async {
        let mut p = connected().await;
        for code in [4, 5, 6] {
            p.dialer.send(&Control::Key { code, pressed: true }).await.unwrap();
        }
        for code in [4, 5, 6] {
            assert_eq!(
                p.listener.recv::<Control>().await.unwrap(),
                Control::Key { code, pressed: true }
            );
        }
        p.listener
            .send(&Control::Key {
                code: 7,
                pressed: false,
            })
            .await
            .unwrap();
        assert_eq!(
            p.dialer.recv::<Control>().await.unwrap(),
            Control::Key {
                code: 7,
                pressed: false
            }
        );
    })
    .await;
}

#[tokio::test]
async fn oversized_frame_is_refused_before_reading_it() {
    within(async {
        let mut p = connected().await;
        let big = Control::Clipboard {
            text: "x".repeat(MAX_FRAME_LEN + 1),
        };
        assert!(matches!(p.dialer.send(&big).await, Err(NetError::Frame(_))));
    })
    .await;
}

#[tokio::test]
async fn motion_datagrams_arrive() {
    within(async {
        let p = connected().await;
        let m = Motion { seq: 42, x: 100, y: -5 };
        // Datagrams may drop; loopback practically never does, but retry to stay deterministic.
        let got = loop {
            p.dialer.send_motion(&m).unwrap();
            if let Ok(Ok(got)) = tokio::time::timeout(Duration::from_millis(200), p.listener.recv_motion()).await {
                break got;
            }
        };
        assert_eq!(got, m);
    })
    .await;
}

#[tokio::test]
async fn peer_going_away_is_noticed() {
    within(async {
        let mut p = connected().await;
        p.dialer.close("bye");
        assert!(matches!(p.listener.recv::<Control>().await, Err(NetError::Closed(_))));
    })
    .await;
}

#[tokio::test]
async fn pairing_shows_the_same_code_and_pins_on_both_confirms() {
    within(async {
        let mut p = connected().await;
        let (dfp, lfp) = (p.dialer_id.fingerprint(), p.listener_id.fingerprint());
        let (codes_tx, mut codes_rx) = tokio::sync::mpsc::unbounded_channel();
        let (tx1, tx2) = (codes_tx.clone(), codes_tx);
        let (d, l) = tokio::join!(
            pair(&mut p.dialer, Role::Dialer, dfp, async move |c| {
                tx1.send(c).unwrap();
                true
            }),
            pair(&mut p.listener, Role::Listener, lfp, async move |c| {
                tx2.send(c).unwrap();
                true
            }),
        );
        assert_eq!(d.unwrap(), lfp);
        assert_eq!(l.unwrap(), dfp);
        assert_eq!(codes_rx.recv().await, codes_rx.recv().await);
    })
    .await;
}

#[tokio::test]
async fn pairing_fails_if_either_user_declines() {
    within(async {
        let mut p = connected().await;
        let (dfp, lfp) = (p.dialer_id.fingerprint(), p.listener_id.fingerprint());
        let (d, l) = tokio::join!(
            pair(&mut p.dialer, Role::Dialer, dfp, async |_| true),
            pair(&mut p.listener, Role::Listener, lfp, async |_| false),
        );
        assert!(matches!(d, Err(SessionError::Declined)));
        assert!(matches!(l, Err(SessionError::Declined)));
    })
    .await;
}

#[tokio::test]
async fn untrusted_peer_cannot_send_input() {
    within(async {
        let mut p = connected().await;
        let lfp = p.listener_id.fingerprint();
        p.dialer.send(&Control::Key { code: 4, pressed: true }).await.unwrap();
        let result = pair(&mut p.listener, Role::Listener, lfp, async |_| true).await;
        assert!(result.is_err(), "a Control frame must never pass the pairing gate");
    })
    .await;
}

#[tokio::test]
async fn hello_exchanges_and_checks_identity() {
    within(async {
        let mut p = connected().await;
        let (mac, pc) = (me("mac"), me("pc"));
        let (d, l) = tokio::join!(hello(&mut p.dialer, &mac), hello(&mut p.listener, &pc));
        let Control::Hello { name, peer, .. } = d.unwrap() else {
            panic!()
        };
        assert_eq!((name.as_str(), peer), ("pc", peer_id(&p.listener_id.fingerprint())));
        assert!(matches!(l.unwrap(), Control::Hello { .. }));
    })
    .await;
}

#[tokio::test]
async fn hello_with_wrong_version_or_forged_id_is_rejected() {
    within(async {
        let mut p = connected().await;
        let forged = Control::Hello {
            version: PROTOCOL_VERSION,
            peer: overdesk_proto::PeerId([0xAA; 16]),
            name: "evil".into(),
            os: Os::Windows,
            width: 1,
            height: 1,
        };
        p.dialer.send(&forged).await.unwrap();
        assert!(matches!(
            hello(&mut p.listener, &me("pc")).await,
            Err(SessionError::IdentityMismatch)
        ));

        let mut p = connected().await;
        let old = Control::Hello {
            version: PROTOCOL_VERSION + 1,
            peer: peer_id(&p.dialer_id.fingerprint()),
            name: "future".into(),
            os: Os::Windows,
            width: 1,
            height: 1,
        };
        p.dialer.send(&old).await.unwrap();
        assert!(matches!(
            hello(&mut p.listener, &me("pc")).await,
            Err(SessionError::VersionMismatch { .. })
        ));
    })
    .await;
}

#[tokio::test]
async fn pairing_message_type_is_distinct_on_the_wire() {
    // Guards the gate above: a Pairing frame is not a Control frame.
    within(async {
        let mut p = connected().await;
        p.dialer.send(&Pairing::Confirm).await.unwrap();
        assert!(p.listener.recv::<Control>().await.is_err());
    })
    .await;
}
