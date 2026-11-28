// SPDX-License-Identifier: AGPL-3.0-only
//! Two real QUIC peers over 127.0.0.1.

use std::future::Future;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use overdesk_net::pairing::peer_id;
use overdesk_net::session::{Me, SessionError, hello, hello_with_timeout, pair, pair_with_timeout};
use overdesk_net::transport::{CloseReason, Identity, Link, NetError, Node, Role};
use overdesk_proto::{Control, FrameError, MAX_FRAME_LEN, Motion, Os, PROTOCOL_VERSION, Pairing};

/// Every test is bounded: a hang is a failure, not a stuck CI job.
async fn within<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out")
}

/// Two connected links over loopback, with the identities behind them.
struct Pair {
    /// The side that dialed.
    dialer: Link,
    /// The side that accepted.
    listener: Link,
    /// The dialer's identity, for checking fingerprints.
    dialer_id: Identity,
    /// The listener's identity, for checking fingerprints.
    listener_id: Identity,
    /// Keeps both endpoints alive; dropping a node would close its links.
    _nodes: (Node, Node),
}

/// Binds two nodes on ephemeral ports and connects them. Neither link is trusted yet.
async fn connected() -> Pair {
    connected_with([false, false]).await
}

/// Like [`connected`], with the transfer option set per side: `[dialer, listener]`.
async fn connected_with(transfers: [bool; 2]) -> Pair {
    let (dialer_id, listener_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let a = Node::bind_with_transfers(&dialer_id, "127.0.0.1:0".parse().unwrap(), transfers[0]).unwrap();
    let b = Node::bind_with_transfers(&listener_id, "127.0.0.1:0".parse().unwrap(), transfers[1]).unwrap();
    let addr = b.local_addr().unwrap();
    let (dialer, listener) = tokio::join!(a.connect(addr), async { b.accept().await.unwrap().await });
    Pair {
        dialer: dialer.unwrap(),
        listener: listener.unwrap(),
        dialer_id,
        listener_id,
        _nodes: (a, b),
    }
}

/// Both sides already paired: each trusts exactly the other's fingerprint.
async fn trusted() -> Pair {
    trusted_with([false, false]).await
}

/// Like [`trusted`], with the transfer option set per side: `[dialer, listener]`.
async fn trusted_with(transfers: [bool; 2]) -> Pair {
    let mut p = connected_with(transfers).await;
    let (dfp, lfp) = (p.dialer_id.fingerprint(), p.listener_id.fingerprint());
    p.dialer.trust(|fp| *fp == lfp).unwrap();
    p.listener.trust(|fp| *fp == dfp).unwrap();
    p
}

/// A `Me` with the given name and fixed placeholder OS and screen size.
fn me(name: &str) -> Me {
    Me {
        name: name.into(),
        os: Os::MacOs,
        width: 1920,
        height: 1080,
        features: 0,
    }
}

#[tokio::test]
async fn both_sides_see_the_other_certificate_and_their_role() {
    within(async {
        let p = connected().await;
        assert_eq!(p.dialer.peer_fingerprint(), p.listener_id.fingerprint());
        assert_eq!(p.listener.peer_fingerprint(), p.dialer_id.fingerprint());
        assert_eq!((p.dialer.role(), p.listener.role()), (Role::Dialer, Role::Listener));
    })
    .await;
}

#[tokio::test]
async fn untrusted_link_refuses_control_and_the_motion_halves() {
    within(async {
        let mut p = connected().await;
        let key = Control::Key { code: 4, pressed: true };
        assert!(matches!(p.dialer.send_control(&key).await, Err(NetError::Untrusted)));
        assert!(matches!(p.listener.recv_control().await, Err(NetError::Untrusted)));
        // Motion and transfers exist only on the halves of a split, which an untrusted link refuses.
        assert!(matches!(p.dialer.split(), Err(NetError::Untrusted)));
    })
    .await;
}

#[tokio::test]
async fn trust_refuses_an_unpinned_fingerprint() {
    within(async {
        let mut p = connected().await;
        assert!(matches!(p.dialer.trust(|_| false), Err(NetError::Untrusted)));
        let key = Control::Key { code: 4, pressed: true };
        assert!(matches!(p.dialer.send_control(&key).await, Err(NetError::Untrusted)));
    })
    .await;
}

#[tokio::test]
async fn control_frames_flow_both_ways_in_order() {
    within(async {
        let mut p = trusted().await;
        for code in [4, 5, 6] {
            p.dialer
                .send_control(&Control::Key { code, pressed: true })
                .await
                .unwrap();
        }
        for code in [4, 5, 6] {
            assert_eq!(
                p.listener.recv_control().await.unwrap(),
                Control::Key { code, pressed: true }
            );
        }
        p.listener
            .send_control(&Control::Key {
                code: 7,
                pressed: false,
            })
            .await
            .unwrap();
        assert_eq!(
            p.dialer.recv_control().await.unwrap(),
            Control::Key {
                code: 7,
                pressed: false
            }
        );
    })
    .await;
}

#[tokio::test]
async fn oversized_frame_is_refused_before_sending() {
    within(async {
        let mut p = trusted().await;
        let big = Control::Clipboard {
            text: "x".repeat(MAX_FRAME_LEN + 1),
        };
        assert!(matches!(p.dialer.send_control(&big).await, Err(NetError::Frame(_))));
    })
    .await;
}

#[tokio::test]
async fn motion_datagrams_arrive() {
    within(async {
        let p = trusted().await;
        let (dtx, _drx) = p.dialer.split().unwrap();
        let (_ltx, lrx) = p.listener.split().unwrap();
        let lmotion = lrx.motion();
        let m = Motion { seq: 42, x: 100, y: -5 };
        // Datagrams may drop; loopback practically never does, but retry to stay deterministic.
        let got = loop {
            dtx.send_motion(&m).unwrap();
            if let Ok(Ok(got)) = tokio::time::timeout(Duration::from_millis(200), lmotion.recv()).await {
                break got;
            }
        };
        assert_eq!(got, m);
    })
    .await;
}

#[tokio::test]
async fn a_motion_sender_works_without_the_writer_half() {
    within(async {
        let p = trusted().await;
        let (tx, _rx) = p.dialer.split().unwrap();
        let (_ltx, lrx) = p.listener.split().unwrap();
        let sender = tx.motion_sender();
        drop(tx); // the handle is independent of the half it came from
        let lmotion = lrx.motion();
        let m = Motion { seq: 7, x: 1, y: 2 };
        let got = loop {
            sender.send(&m).unwrap();
            if let Ok(Ok(got)) = tokio::time::timeout(Duration::from_millis(200), lmotion.recv()).await {
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
        let mut p = trusted().await;
        p.dialer.close(CloseReason::Busy);
        let e = p.listener.recv_control().await.unwrap_err();
        assert!(matches!(e, NetError::Refused(CloseReason::Busy)), "{e}");
    })
    .await;
}

#[tokio::test]
async fn a_deliberate_leave_is_told_apart_from_a_lost_link() {
    within(async {
        let p = trusted().await;
        let (tx, _rx) = p.dialer.split().unwrap();
        tx.closer().leave();
        let (_tx, mut rx) = p.listener.split().unwrap();
        assert!(matches!(rx.recv_control().await, Err(NetError::PeerLeft)));
    })
    .await;
}

#[tokio::test]
async fn forgetting_is_told_apart_from_leaving() {
    within(async {
        let p = trusted().await;
        let (tx, _rx) = p.dialer.split().unwrap();
        tx.closer().forget();
        let (_tx, mut rx) = p.listener.split().unwrap();
        assert!(matches!(rx.recv_control().await, Err(NetError::Forgotten)));
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
            pair(&mut p.dialer, async move |c| {
                tx1.send(c).unwrap();
                true
            }),
            pair(&mut p.listener, async move |c| {
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
        let (d, l) = tokio::join!(
            pair(&mut p.dialer, async |_| true),
            pair(&mut p.listener, async |_| false)
        );
        assert!(matches!(d, Err(SessionError::Declined)));
        assert!(matches!(l, Err(SessionError::Declined)));
    })
    .await;
}

#[tokio::test]
async fn untrusted_peer_cannot_send_input() {
    within(async {
        // Even a dialer that (wrongly) trusts, sending real Control, cannot get past
        // a listener that is pairing: those bytes never decode as Pairing.
        let mut p = connected().await;
        p.dialer.trust(|_| true).unwrap();
        p.dialer
            .send_control(&Control::Key { code: 4, pressed: true })
            .await
            .unwrap();
        assert!(pair(&mut p.listener, async |_| true).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn pre_pairing_frames_are_tiny() {
    within(async {
        let mut p = connected().await;
        p.dialer.trust(|_| true).unwrap();
        p.dialer
            .send_control(&Control::Clipboard { text: "x".repeat(1000) })
            .await
            .unwrap();
        let r = pair(&mut p.listener, async |_| true).await;
        assert!(
            matches!(r, Err(SessionError::Net(NetError::Frame(FrameError::TooLarge(_))))),
            "{r:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn hello_exchanges_and_checks_identity() {
    within(async {
        let mut p = trusted().await;
        let (mac, mut pc) = (me("mac"), me("pc"));
        pc.features = overdesk_proto::FEATURE_KEY_TEXT | 1 << 40; // an unknown bit is carried, not rejected
        let (d, l) = tokio::join!(hello(&mut p.dialer, &mac), hello(&mut p.listener, &pc));
        let overdesk_net::session::PeerHello {
            name, peer, features, ..
        } = d.unwrap();
        assert_eq!(features, overdesk_proto::FEATURE_KEY_TEXT | 1 << 40);
        assert_eq!((name.as_str(), peer), ("pc", peer_id(&p.listener_id.fingerprint())));
        assert_eq!(l.unwrap().name, "mac");
    })
    .await;
}

#[tokio::test]
async fn hello_requires_trust() {
    within(async {
        let mut p = connected().await;
        assert!(matches!(
            hello(&mut p.dialer, &me("mac")).await,
            Err(SessionError::Net(NetError::Untrusted))
        ));
    })
    .await;
}

#[tokio::test]
async fn hello_with_wrong_version_or_forged_id_is_rejected() {
    within(async {
        let mut p = trusted().await;
        let forged = Control::Hello {
            version: PROTOCOL_VERSION,
            peer: overdesk_proto::PeerId([0xAA; 16]),
            name: "evil".into(),
            os: Os::Windows,
            width: 1,
            height: 1,
            features: 0,
        };
        p.dialer.send_control(&forged).await.unwrap();
        assert!(matches!(
            hello(&mut p.listener, &me("pc")).await,
            Err(SessionError::IdentityMismatch)
        ));

        let mut p = trusted().await;
        let future = Control::Hello {
            version: PROTOCOL_VERSION + 1,
            peer: peer_id(&p.dialer_id.fingerprint()),
            name: "future".into(),
            os: Os::Windows,
            width: 1,
            height: 1,
            features: 0,
        };
        p.dialer.send_control(&future).await.unwrap();
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
        let mut p = trusted().await;
        p.dialer.send_pairing(&Pairing::Confirm).await.unwrap();
        assert!(p.listener.recv_control().await.is_err());
    })
    .await;
}

#[tokio::test]
async fn silent_peer_cannot_hold_pairing_open() {
    within(async {
        let mut p = connected().await;
        let r = pair_with_timeout(&mut p.listener, async |_| true, Duration::from_millis(300)).await;
        assert!(matches!(r, Err(SessionError::TimedOut)), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn split_halves_send_and_receive_concurrently() {
    within(async {
        let p = trusted().await;
        let (dtx, mut drx) = p.dialer.split().unwrap();
        let (ltx, mut lrx) = p.listener.split().unwrap();
        let (mut dtx, mut ltx) = (dtx, ltx);
        // Both readers park first, then both writers send: no lock-step needed.
        let readers = tokio::spawn(async move {
            let a = lrx.recv_control().await.unwrap();
            let b = drx.recv_control().await.unwrap();
            (a, b)
        });
        dtx.send_control(&Control::Key { code: 4, pressed: true })
            .await
            .unwrap();
        ltx.send_control(&Control::Key { code: 5, pressed: true })
            .await
            .unwrap();
        let (a, b) = readers.await.unwrap();
        assert_eq!(a, Control::Key { code: 4, pressed: true });
        assert_eq!(b, Control::Key { code: 5, pressed: true });
        dtx.send_motion(&Motion { seq: 1, x: 2, y: 3 }).unwrap();
    })
    .await;
}

#[tokio::test]
async fn only_trusted_links_split() {
    // Transfers and motion exist only on the halves, so refusing the split gates them too.
    within(async {
        let p = connected_with([true, true]).await;
        assert!(matches!(p.dialer.split(), Err(NetError::Untrusted)));
        assert!(matches!(p.listener.split(), Err(NetError::Untrusted)));
    })
    .await;
}

#[tokio::test]
async fn a_32_mib_transfer_does_not_starve_control() {
    const TOTAL: usize = 32 * 1024 * 1024;
    let p = trusted_with([true, true]).await;
    let (mut dtx, _drx) = p.dialer.split().unwrap();
    let (_ltx, mut lrx) = p.listener.split().unwrap();
    let incoming = lrx.transfers();
    let id = [7u8; 16];
    let writer = dtx.open_transfer(id).await.unwrap();
    let sender = tokio::spawn(async move {
        let mut w = writer;
        let chunk = vec![0xA5u8; 64 * 1024];
        for _ in 0..TOTAL / chunk.len() {
            w.write_all(&chunk).await.unwrap();
        }
        w.shutdown().await.unwrap();
    });
    let receiver = tokio::spawn(async move {
        let (got, mut r) = incoming.accept().await.unwrap();
        let mut all = Vec::new();
        r.read_to_end(&mut all).await.unwrap();
        (got, all)
    });
    // Control keeps flowing while the bytes move: each round trip stays well under a second.
    let started = Instant::now();
    let mut slowest = Duration::ZERO;
    while !sender.is_finished() {
        let t = Instant::now();
        dtx.send_control(&Control::Key { code: 4, pressed: true })
            .await
            .unwrap();
        assert_eq!(
            lrx.recv_control().await.unwrap(),
            Control::Key { code: 4, pressed: true }
        );
        slowest = slowest.max(t.elapsed());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(slowest < Duration::from_millis(500), "control stalled for {slowest:?}");
    sender.await.unwrap();
    let (got, all) = receiver.await.unwrap();
    assert_eq!((got, all.len()), (id, TOTAL));
    assert!(all.iter().all(|b| *b == 0xA5));
    println!("32 MiB in {:?}, slowest control hop {slowest:?}", started.elapsed());
}

#[tokio::test]
async fn a_free_node_refuses_transfer_streams() {
    let p = trusted_with([true, false]).await;
    let (dtx, _drx) = p.dialer.split().unwrap();
    // The dialer allows transfers, the listener is a default (Free) node: it grants no stream credit.
    let opened = tokio::time::timeout(Duration::from_millis(500), dtx.open_transfer([1; 16])).await;
    assert!(opened.is_err(), "a Free node accepted a transfer stream");
}

#[tokio::test]
async fn silent_peer_cannot_hold_hello_open() {
    within(async {
        let mut p = trusted().await;
        let r = hello_with_timeout(&mut p.listener, &me("pc"), Duration::from_millis(300)).await;
        assert!(matches!(r, Err(SessionError::TimedOut)), "{r:?}");
        // The timeout closed the link, so the other side notices.
        while p.dialer.recv_control().await.is_ok() {} // its own Hello may still arrive first
    })
    .await;
}

#[tokio::test]
async fn hello_name_is_sanitised_and_screen_size_checked() {
    within(async {
        let mut p = trusted().await;
        let sneaky = Control::Hello {
            version: PROTOCOL_VERSION,
            peer: peer_id(&p.dialer_id.fingerprint()),
            name: format!("Work\u{202E}\u{200B}\n PC{}", "x".repeat(200)),
            os: Os::Windows,
            width: 1,
            height: 65_535,
            features: 0,
        };
        p.dialer.send_control(&sneaky).await.unwrap();
        let name = hello(&mut p.listener, &me("pc")).await.unwrap().name;
        assert!(name.starts_with("Work PCxxx") && name.chars().count() == 64, "{name:?}");

        for (width, height) in [(0, 10), (10, 0), (65_536, 10), (10, u32::MAX)] {
            let mut p = trusted().await;
            let bad = Control::Hello {
                version: PROTOCOL_VERSION,
                peer: peer_id(&p.dialer_id.fingerprint()),
                name: "bad".into(),
                os: Os::Windows,
                width,
                height,
                features: 0,
            };
            p.dialer.send_control(&bad).await.unwrap();
            let r = hello(&mut p.listener, &me("pc")).await;
            assert!(
                matches!(r, Err(SessionError::BadScreen { .. })),
                "{width}x{height}: {r:?}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn a_sender_dropped_mid_transfer_looks_aborted_not_complete() {
    within(async {
        let p = trusted_with([true, true]).await;
        let (dtx, _drx) = p.dialer.split().unwrap();
        let (_ltx, lrx) = p.listener.split().unwrap();
        let incoming = lrx.transfers();
        let mut w = dtx.open_transfer([9; 16]).await.unwrap();
        w.write_all(b"partial").await.unwrap();
        w.flush().await.unwrap();
        let (id, mut r) = incoming.accept().await.unwrap();
        assert_eq!(id, [9; 16]);
        drop(w); // aborted: never shut down
        let mut all = Vec::new();
        assert!(
            r.read_to_end(&mut all).await.is_err(),
            "an aborted transfer read as complete"
        );
    })
    .await;
}

#[tokio::test]
async fn a_shut_down_transfer_reads_as_complete() {
    within(async {
        let p = trusted_with([true, true]).await;
        let (dtx, _drx) = p.dialer.split().unwrap();
        let (_ltx, lrx) = p.listener.split().unwrap();
        let incoming = lrx.transfers();
        let mut w = dtx.open_transfer([9; 16]).await.unwrap();
        w.write_all(b"whole").await.unwrap();
        w.shutdown().await.unwrap();
        drop(w);
        let (_, mut r) = incoming.accept().await.unwrap();
        let mut all = Vec::new();
        r.read_to_end(&mut all).await.unwrap();
        assert_eq!(all, b"whole");
    })
    .await;
}

#[tokio::test]
async fn a_stalled_send_times_out_and_closes_the_link() {
    within(async {
        let p = trusted().await;
        let (mut dtx, _drx) = p.dialer.split().unwrap();
        let _stalled = p.listener; // never reads
        let big = Control::Clipboard {
            text: "x".repeat(MAX_FRAME_LEN - 64),
        };
        let r: Result<(), NetError> = async {
            loop {
                dtx.send_control_timeout(&big, Duration::from_millis(500)).await?;
            }
        }
        .await;
        assert!(matches!(r, Err(NetError::Closed(_))), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn our_own_impossible_screen_is_not_announced() {
    within(async {
        let mut p = trusted().await;
        let mut bad = me("mac");
        bad.width = 0;
        let r = hello(&mut p.dialer, &bad).await;
        assert!(matches!(r, Err(SessionError::OwnScreen { width: 0, .. })), "{r:?}");
    })
    .await;
}
