// SPDX-License-Identifier: AGPL-3.0-only
//! Wire codec cost in ns/op, to compare before and after a change to it.
//!
//!   cargo run --release -p overdesk-proto --example codec
//!
//! Run it on a quiet machine, a few times: the numbers move with load and thermals.

use std::hint::black_box;
use std::time::Instant;

use overdesk_proto::{Control, Motion, decode_frame, decode_motion, encode_frame, encode_motion};

/// Prints the mean time of `op` over `iters` runs, after a warm-up of a tenth as many.
fn bench<T>(name: &str, iters: u32, mut op: impl FnMut() -> T) {
    for _ in 0..iters / 10 {
        black_box(op());
    }
    let start = Instant::now();
    for _ in 0..iters {
        black_box(op());
    }
    println!(
        "{name:<28} {:>10.1} ns/op",
        start.elapsed().as_nanos() as f64 / f64::from(iters)
    );
}

fn main() {
    let motion = Motion {
        seq: 1_234_567,
        x: -1920,
        y: 1080,
    };
    let key = Control::Key {
        code: 0xE3,
        pressed: true,
    };
    let text = Control::Clipboard { text: "x".repeat(4096) };
    let big = Control::Clipboard {
        text: "x".repeat(1024 * 1024 - 64),
    };
    let datagram = encode_motion(&motion);
    let frame = encode_frame(&key).unwrap();
    bench("encode_motion", 5_000_000, || encode_motion(black_box(&motion)));
    bench("decode_motion", 5_000_000, || decode_motion(black_box(&datagram)));
    bench("encode_frame(Key)", 5_000_000, || encode_frame(black_box(&key)));
    bench("decode_frame(Key)", 5_000_000, || {
        decode_frame::<Control>(black_box(&frame))
    });
    bench("encode_frame(Clipboard 4K)", 200_000, || encode_frame(black_box(&text)));
    bench("encode_frame(Clipboard 1M)", 500, || encode_frame(black_box(&big)));
}
