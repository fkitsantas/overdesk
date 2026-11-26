// SPDX-License-Identifier: AGPL-3.0-only
//! Hand test for the macOS adapter. Never swallows input and always exits by itself.
//!
//!   cargo run -p overdesk-platform --example mac_probe -- listen [secs]   # print captured input (default 10 s)
//!   cargo run -p overdesk-platform --example mac_probe -- inject   # draw a small square, scroll, tap Shift
//!   cargo run -p overdesk-platform --example mac_probe -- jump     # inject a jump, then print your next real moves

#[cfg(target_os = "macos")]
fn main() {
    use overdesk_platform::mac::ffi::{Capture, Injector, permissions, request_permissions};
    use overdesk_proto::{Phase, ScrollUnit};
    use std::time::{Duration, Instant};

    let (listen, post) = permissions();
    println!("Input Monitoring: {listen}  Accessibility: {post}");
    if !(listen && post) {
        request_permissions();
        println!("Grant both to this terminal in System Settings › Privacy & Security, then re-run.");
        return;
    }
    match std::env::args().nth(1).as_deref() {
        Some("listen") => {
            let (_capture, rx) = Capture::start().expect("tap");
            let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            let end = Instant::now() + Duration::from_secs(secs);
            println!("Listening for {secs} s (input still reaches apps)...");
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                if let Ok(ev) = rx.recv_timeout(left) {
                    println!("{ev:?}");
                }
            }
        }
        Some("inject") => {
            let mut inj = Injector::new().expect("injector");
            let corners = [
                (400.0, 400.0),
                (500.0, 400.0),
                (500.0, 500.0),
                (400.0, 500.0),
                (400.0, 400.0),
            ];
            for w in corners.windows(2) {
                let ((x0, y0), (x1, y1)) = (w[0], w[1]);
                for i in 0..=50 {
                    let t = i as f64 / 50.0;
                    inj.pointer(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
                    std::thread::sleep(Duration::from_millis(4));
                }
                std::thread::sleep(Duration::from_millis(30));
                let (cx, cy) = Injector::cursor();
                let ok = (cx - x1).abs() < 1.0 && (cy - y1).abs() < 1.0;
                println!(
                    "corner ({x1}, {y1}) -> cursor at ({cx:.0}, {cy:.0}) {}",
                    if ok { "OK" } else { "MISMATCH" }
                );
            }
            let phases = [
                Phase::Began,
                Phase::Changed,
                Phase::Changed,
                Phase::Ended,
                Phase::MomentumBegan,
                Phase::Momentum,
                Phase::MomentumEnded,
            ];
            for p in phases {
                inj.scroll(0, -12, ScrollUnit::Pixel, p);
                std::thread::sleep(Duration::from_millis(16));
            }
            inj.key(0xE1, true);
            inj.key(0xE1, false);
            println!("Injected square, pixel scroll with momentum, Shift tap.");
        }
        Some("jump") => {
            // Does the first real delta after an injected jump carry the jump itself?
            let (_capture, rx) = Capture::start().expect("tap");
            let mut inj = Injector::new().expect("injector");
            inj.pointer(300.0, 300.0);
            inj.pointer(1300.0, 800.0);
            println!("Injected jump (300,300) -> (1300,800). Nudge the mouse slightly within 8 s...");
            let end = Instant::now() + Duration::from_secs(8);
            let mut seen = 0;
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                if let Ok(ev @ overdesk_platform::input::Captured::Moved { .. }) = rx.recv_timeout(left) {
                    println!("{ev:?}");
                    seen += 1;
                    if seen == 5 {
                        break;
                    }
                }
            }
        }
        _ => println!("usage: mac_probe listen|inject|jump"),
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("mac_probe runs on macOS only");
}
