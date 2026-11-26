// SPDX-License-Identifier: AGPL-3.0-only
//! Hand test for the Linux X11 adapter. Never swallows input and always exits by itself.
//!
//!   cargo run -p overdesk-platform --example linux_probe -- listen [secs]   # print captured input (default 10 s)
//!   cargo run -p overdesk-platform --example linux_probe -- inject   # draw a small square, scroll, tap Shift
//!   cargo run -p overdesk-platform --example linux_probe -- roundtrip   # inject, then check capture saw it
//!       (needs OVERDESK_X11_CAPTURE_XTEST=1: capture ignores XTest input otherwise)
//!   cargo run -p overdesk-platform --example linux_probe -- swallow   # grab for 3 s, then release

/// Runs the subcommand named by the first argument.
#[cfg(target_os = "linux")]
fn main() {
    use overdesk_platform::input::Captured;
    use overdesk_platform::linux::ffi::{Capture, Injector};
    use overdesk_proto::{Button, Phase, ScrollUnit};
    use std::time::{Duration, Instant};

    /// Draws the square, a click, a scroll and a Shift tap with `inj`.
    fn demo(inj: &mut Injector) {
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
                let t = f64::from(i) / 50.0;
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
        inj.button(Button::Left, true);
        inj.button(Button::Left, false);
        inj.scroll(0, -120, ScrollUnit::Pixel, Phase::Changed);
        inj.scroll(60, 0, ScrollUnit::Pixel, Phase::Changed);
        inj.key(0xE1, true);
        inj.key(0xE1, false);
        inj.key(0x04, true);
        inj.key(0x04, false);
    }

    match std::env::args().nth(1).as_deref() {
        Some("listen") => {
            let (_capture, rx) = Capture::start().expect("capture");
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
            demo(&mut Injector::new().expect("injector"));
            println!("Injected square, click, scroll, Shift and A taps.");
        }
        Some("roundtrip") => {
            let (_capture, rx) = Capture::start().expect("capture");
            demo(&mut Injector::new().expect("injector"));
            // The capture thread queries the pointer per move, so let it drain until idle.
            let mut events = Vec::new();
            while let Ok(e) = rx.recv_timeout(Duration::from_millis(500)) {
                events.push(e);
            }
            let count = |f: fn(&Captured) -> bool| events.iter().filter(|e| f(e)).count();
            let moved = count(|e| matches!(e, Captured::Moved { .. }));
            let buttons = count(|e| matches!(e, Captured::Button { .. }));
            let scrolls = count(|e| matches!(e, Captured::Scroll { .. }));
            let keys = count(|e| matches!(e, Captured::Key { .. }));
            println!("captured: {moved} moves, {buttons} button edges, {scrolls} scrolls, {keys} key edges");
            for e in events.iter().filter(|e| !matches!(e, Captured::Moved { .. })) {
                println!("{e:?}");
            }
            let ok = moved > 0 && buttons == 2 && scrolls == 3 && keys == 4;
            println!("roundtrip {}", if ok { "OK" } else { "FAILED" });
            std::process::exit(i32::from(!ok));
        }
        Some("swallow") => {
            let (capture, rx) = Capture::start().expect("capture");
            capture.set_swallow(true);
            println!("Input is grabbed for 3 s (Ctrl+Alt+Super+Esc releases it)...");
            let end = Instant::now() + Duration::from_secs(3);
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                if let Ok(ev) = rx.recv_timeout(left) {
                    println!("{ev:?}");
                    if ev == Captured::Escape {
                        break;
                    }
                }
            }
            capture.set_swallow(false);
            println!("Released.");
        }
        _ => println!("usage: linux_probe listen|inject|roundtrip|swallow"),
    }
}

/// Stub so the example builds, and `--all-targets` checks pass, on other systems.
#[cfg(not(target_os = "linux"))]
fn main() {
    println!("linux_probe runs on Linux only");
}
