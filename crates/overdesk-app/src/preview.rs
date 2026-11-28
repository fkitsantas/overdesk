// SPDX-License-Identifier: AGPL-3.0-only
//! Design review only (`--features preview`): render one fixed state, save a screenshot
//! of the window to `OVERDESK_SHOT`, exit. Never part of a shipped build.

use std::time::Duration;

use iced::Task;
use iced::theme::Mode as ThemeMode;
use overdesk_proto::{Os, PeerId};
use overdeskd::Mode;
use overdeskd::controller::{AppState, DeviceKind, NearbyDevice, PeerView, Phase, ScreenView};

use crate::Message;

/// The fixed state named by `which`, and the permission flags to show (if the
/// permissions screen is wanted instead).
pub fn state(which: &str) -> (AppState, Option<(bool, bool)>) {
    let (me, peer) = (PeerId([1; 16]), PeerId([2; 16]));
    let desk = vec![
        ScreenView {
            id: me,
            name: "MacBook Pro".into(),
            x: 0,
            y: 0,
            w: 3840,
            h: 2160,
            me: true,
            online: true,
            kind: DeviceKind::Computer,
        },
        ScreenView {
            id: peer,
            name: "DESKTOP-7Q2K4LM".into(),
            x: 3840,
            y: 0,
            w: 1920,
            h: 1080,
            me: false,
            online: true,
            kind: DeviceKind::Computer,
        },
    ];
    let third = PeerId([5; 16]);
    let mut peers = vec![PeerView {
        id: peer,
        name: "DESKTOP-7Q2K4LM".into(),
        os: Os::Windows,
        features: 0,
        kind: DeviceKind::Computer,
        label: None,
    }];
    let mut desk = desk;
    if which == "arrange" || which == "three" {
        // A Linux laptop below the MacBook, as a third computer.
        peers.push(PeerView {
            id: third,
            name: "fedora".into(),
            os: Os::Linux,
            features: 0,
            kind: DeviceKind::Computer,
            label: None,
        });
        desk.push(ScreenView {
            id: third,
            name: "fedora".into(),
            x: 0,
            y: 2160,
            w: 1920,
            h: 1080,
            me: false,
            online: true,
            kind: DeviceKind::Computer,
        });
    }
    let connected = |mode| Phase::Connected {
        peers: peers.clone(),
        mode,
    };
    let (phase, desk, perms) = match which {
        "permissions" => (Phase::Idle, vec![], Some((true, false))),
        "connecting" => (Phase::Connecting { address: "192.168.1.50:24801".parse().unwrap() }, vec![], None),
        "pairing" => (Phase::Pairing { peer: "192.168.1.50".into(), code: 382930 }, vec![], None),
        "local" => (connected(Mode::Local), desk, None),
        "driving" | "three" | "arrange" => (connected(Mode::Driving(peer)), desk, None),
        "driven" => (connected(Mode::Driven(peer)), desk, None),
        "lost" => (Phase::Lost { peer: "DESKTOP-7Q2K4LM".into() }, vec![], None),
        "error" => (Phase::Error("Couldn't connect: timed out. Check that Overdesk is open on the other computer and that both are on the same network.".into()), vec![], None),
        _ => (Phase::Idle, vec![], None),
    };
    let state = AppState {
        address: Some("0.0.0.0:24801".parse().unwrap()),
        phase,
        desk,
        // The cursor is on the peer only while this device drives it.
        focus: matches!(which, "local" | "driving" | "driven" | "three" | "arrange").then_some(
            if matches!(which, "driving" | "three" | "arrange") {
                peer
            } else {
                me
            },
        ),
        can_arrange: which == "arrange",
        pairing_open: which == "pairing-open",
        nearby: if which == "idle" {
            vec![
                NearbyDevice {
                    kind: DeviceKind::Computer,
                    id: PeerId([3; 16]),
                    name: "Mac Studio".into(),
                    os: Os::MacOs,
                    address: "192.168.1.20:24801".parse().unwrap(),
                    paired: true,
                },
                NearbyDevice {
                    kind: DeviceKind::Computer,
                    id: PeerId([4; 16]),
                    name: "DESKTOP-7Q2K4LM".into(),
                    os: Os::Windows,
                    address: "192.168.1.50:24801".parse().unwrap(),
                    paired: false,
                },
            ]
        } else {
            vec![]
        },
        ..AppState::new("MacBook Pro".into(), Os::MacOs)
    };
    (state, perms)
}

/// The licence row to preview for `which`, if it is a licence state.
pub fn edition(which: &str) -> Option<crate::Edition> {
    matches!(which, "licence" | "licence-error" | "licensed").then(|| crate::Edition {
        licensed_to: (which == "licensed").then(|| "ada@example.org".into()),
        activate: || crate::ActivationStart {
            url: "https://overdesk.org/activate".into(),
            code: "AB12-CD34".into(),
            done: Box::pin(std::future::pending()),
        },
        report: crate::free_report,
    })
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
