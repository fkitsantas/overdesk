// SPDX-License-Identifier: AGPL-3.0-only
//! Two controllers (what the app runs) over real loopback QUIC, on fake platforms.

use std::path::PathBuf;
use std::time::Duration;

use overdesk_core::engine::{Action, Mode};
use overdesk_net::session::Me;
use overdesk_proto::Os;
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
        })
        .unwrap();
    wait(&mut a, "A refused", |p| matches!(p, Phase::Error(_))).await;
    assert_eq!(b.borrow().phase, Phase::Idle, "B never showed a code");
}
