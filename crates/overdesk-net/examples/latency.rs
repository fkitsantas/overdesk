// SPDX-License-Identifier: AGPL-3.0-only
//! Motion-lane round-trip latency over loopback QUIC.
//!
//!   cargo run --release -p overdesk-net --example latency

use std::time::{Duration, Instant};

use overdesk_net::transport::{Identity, Node};
use overdesk_proto::Motion;

/// Number of round trips to measure.
const SAMPLES: usize = 2000;

/// Connects two nodes over loopback, echoes motion datagrams from one to the other,
/// and prints the p50, p99 and maximum round-trip times and the number of samples lost.
#[tokio::main]
async fn main() {
    let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let a = Node::bind(&a_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let b = Node::bind(&b_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = b.local_addr().unwrap();
    let (dialer, listener) = tokio::join!(a.connect(addr), async { b.accept().await.unwrap().await });
    let (mut dialer, mut listener) = (dialer.unwrap(), listener.unwrap());
    dialer.trust(|_| true).unwrap(); // benchmark only: no pairing
    listener.trust(|_| true).unwrap();
    let (dialer, dialer_rx) = dialer.split().unwrap();
    let (listener, listener_rx) = listener.split().unwrap();
    let (listener_rx, dialer_rx) = (listener_rx.motion(), dialer_rx.motion());

    // Echo every datagram straight back.
    tokio::spawn(async move {
        while let Ok(m) = listener_rx.recv().await {
            let _ = listener.send_motion(&m);
        }
    });

    let mut rtts = Vec::with_capacity(SAMPLES);
    let mut lost = 0;
    for seq in 0..SAMPLES as u32 {
        let start = Instant::now();
        dialer.send_motion(&Motion { seq, x: 1, y: 1 }).unwrap();
        // A sample not back within 100 ms counts as lost, as does a late echo of an
        // earlier one.
        match tokio::time::timeout(Duration::from_millis(100), dialer_rx.recv()).await {
            Ok(Ok(m)) if m.seq == seq => rtts.push(start.elapsed()),
            _ => lost += 1,
        }
    }
    rtts.sort();
    let pct = |p: f64| rtts[((rtts.len() as f64 * p) as usize).min(rtts.len() - 1)];
    println!(
        "motion round trip over loopback, {} samples: p50 {:?}  p99 {:?}  max {:?}  lost {lost}",
        rtts.len(),
        pct(0.50),
        pct(0.99),
        rtts[rtts.len() - 1]
    );
}
