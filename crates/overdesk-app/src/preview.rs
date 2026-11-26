// SPDX-License-Identifier: AGPL-3.0-only
//! Design review only (`--features preview`): render one fixed state, save a screenshot
//! of the window to `OVERDESK_SHOT`, exit. Never part of a shipped build.

use std::time::Duration;

use iced::Task;
use iced::theme::Mode as ThemeMode;
use overdesk_core::engine::Mode;
use overdesk_proto::{Os, PeerId};
use overdeskd::controller::{AppState, Phase, ScreenView};

use crate::Message;

/// The fixed state named by `which`, and the permission flags to show (if the
/// permissions screen is wanted instead).
pub fn state(which: &str) -> (AppState, Option<(bool, bool)>) {
    let desk = vec![
        ScreenView {
            name: "MacBook Pro".into(),
            x: 0,
            y: 0,
            w: 3840,
            h: 2160,
            me: true,
        },
        ScreenView {
            name: "Work PC".into(),
            x: 3840,
            y: 0,
            w: 1920,
            h: 1080,
            me: false,
        },
    ];
    let peer = PeerId([2; 16]);
    let connected = |mode| Phase::Connected {
        peer: "Work PC".into(),
        peer_os: Os::Windows,
        mode,
    };
    let (phase, desk, perms) = match which {
        "permissions" => (Phase::Idle, vec![], Some((true, false))),
        "connecting" => (Phase::Connecting { address: "192.168.1.50:24801".parse().unwrap() }, vec![], None),
        "pairing" => (Phase::Pairing { peer: "192.168.1.50".into(), code: 382930 }, vec![], None),
        "local" => (connected(Mode::Local), desk, None),
        "driving" => (connected(Mode::Driving(peer)), desk, None),
        "driven" => (connected(Mode::Driven(peer)), desk, None),
        "lost" => (Phase::Lost { peer: "Work PC".into() }, vec![], None),
        "error" => (Phase::Error("Couldn't connect: timed out. Check that Overdesk is open on the other computer and that both are on the same network.".into()), vec![], None),
        _ => (Phase::Idle, vec![], None),
    };
    let state = AppState {
        name: "MacBook Pro".into(),
        os: Os::MacOs,
        address: Some("0.0.0.0:24801".parse().unwrap()),
        phase,
        desk,
        pairing_open: which == "pairing-open",
    };
    (state, perms)
}

/// Light, or dark when `OVERDESK_PREVIEW_THEME=dark`.
pub fn theme() -> ThemeMode {
    if std::env::var("OVERDESK_PREVIEW_THEME").as_deref() == Ok("dark") {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    }
}

/// Waits for the first frames, screenshots the window, writes a PNG, exits.
pub fn shoot() -> Task<Message> {
    // Give the window time to lay out and paint before capturing it.
    Task::future(tokio::time::sleep(Duration::from_millis(900)))
        .then(|_| iced::window::latest())
        .then(|id| match id {
            Some(id) => iced::window::screenshot(id).map(Message::Shot),
            None => Task::none(),
        })
}

/// Writes the screenshot as a PNG to `OVERDESK_SHOT` and exits.
pub fn save(shot: iced::window::Screenshot) -> ! {
    let path = std::env::var("OVERDESK_SHOT").unwrap_or_else(|_| "overdesk-preview.png".into());
    let file = std::fs::File::create(&path).expect("create screenshot");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), shot.size.width, shot.size.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("png header")
        .write_image_data(&shot.rgba)
        .expect("png data");
    std::process::exit(0)
}
