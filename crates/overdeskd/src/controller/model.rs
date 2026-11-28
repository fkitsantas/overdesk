// SPDX-License-Identifier: AGPL-3.0-only
//! The controller's vocabulary: what the UI sends ([`Command`]), what it renders ([`AppState`]),
//! and the plain data in between.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use overdesk_core::engine::Mode;
use overdesk_net::session::Me;
use overdesk_proto::{Os, PeerId};

use crate::Admit;

/// What the controller needs to start: where to keep state, where to listen, and who
/// this device is.
pub struct Config {
    /// The folder that holds this device's identity and its pinned peers.
    pub store_dir: PathBuf,
    /// The address to accept incoming connections on.
    pub listen: SocketAddr,
    /// This device's name, operating system and screen size, sent to every peer.
    pub me: Me,
    /// Whether to announce this device on the local network and list the others.
    pub discovery: bool,
    /// A generic hook that vets each peer after `Hello` and before the Desk (see [`Admit`]); `None`
    /// admits every pinned peer. Which `FEATURE_*` bits are announced is `me.features`. The Pro
    /// controller does not use this: it vets peers its own way.
    pub admit: Option<Arc<dyn Admit>>,
}

/// Where the controller is in its connection lifecycle; the UI picks its screen from this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Not connected and not trying to; ready for a [`Command::Connect`].
    Idle,
    /// Dialing another device.
    Connecting {
        /// The address being dialed.
        address: SocketAddr,
    },
    /// Show `code`; the user answers with [`Command::AnswerCode`].
    Pairing {
        /// The other device, by IP address (it has not introduced itself yet).
        peer: String,
        /// The six-digit code both screens must show.
        code: u32,
    },
    /// Show a PIN field: a remote device (see [`DeviceKind::Remote`]) is displaying a PIN to
    /// type, which the user answers with [`Command::SubmitPin`]. [`Command::Disconnect`]
    /// cancels. Only Pro sets it; the open controller never does.
    EnterPin {
        /// The device that shows the PIN, by name.
        name: String,
    },
    /// A session is running.
    Connected {
        /// The connected peers: exactly one in the open core.
        peers: Vec<PeerView>,
        /// Where the cursor is: on this device, driving the peer, or driven by it.
        mode: Mode,
    },
    /// The session ended because the peer went away; input is back on this device.
    Lost {
        /// The device name of the peer that was lost.
        peer: String,
    },
    /// Connecting, pairing or setting up the Desk failed, with a message for people.
    Error(String),
}

/// A connected peer, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerView {
    /// The peer's id.
    pub id: PeerId,
    /// The peer's device name.
    pub name: String,
    /// The peer's operating system.
    pub os: Os,
    /// What kind of device it is, so the UI can draw the right glyph.
    pub kind: DeviceKind,
    /// What to call it instead of its operating system (Pro names an "Apple TV"); `None` for a computer.
    pub label: Option<String>,
}

/// One screen of the shared Desk, named for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenView {
    /// The device that owns this screen.
    pub id: PeerId,
    /// The device name shown on the screen.
    pub name: String,
    /// The screen's left edge in Desk coordinates.
    pub x: i32,
    /// The screen's top edge in Desk coordinates.
    pub y: i32,
    /// The screen's width in pixels.
    pub w: u32,
    /// The screen's height in pixels.
    pub h: u32,
    /// Whether this screen belongs to this device.
    pub me: bool,
    /// Whether the device is reachable now. A screen stays on the Desk while its device is
    /// away; the UI shows it dimmed. Always `true` in the open core, which lists connected devices only.
    pub online: bool,
    /// What kind of device owns the screen, so the UI can draw the right glyph.
    pub kind: DeviceKind,
}

/// What a [`NearbyDevice`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// An Overdesk computer: [`Command::Connect`] dials and pairs it.
    Computer,
    /// A device with no Overdesk app that the controller drives some other way (Pro lists
    /// Apple TVs). [`Command::Connect`] with `expect` set to its id pairs it, if it is not
    /// paired, and otherwise connects it; the controller then asks for a PIN with
    /// [`Phase::EnterPin`]. The open controller never lists one.
    Remote {
        /// The controller will not use it in this edition: the UI greys it, saying it needs Overdesk Pro.
        locked: bool,
    },
}

/// Another device seen on the local network: an Overdesk computer, or a [`DeviceKind::Remote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearbyDevice {
    /// What kind of device it is.
    pub kind: DeviceKind,
    /// The device's name.
    pub name: String,
    /// The device's operating system.
    pub os: Os,
    /// Where to dial it; pass it to [`Command::Connect`].
    ///
    /// Only a hint: it comes from an unauthenticated announcement and is verified by the
    /// handshake when connecting (see `Command::Connect`'s `expect`).
    pub address: SocketAddr,
    /// The id the device announced; pass it as `expect` when connecting to a paired one.
    pub id: PeerId,
    /// Whether a pinned device has this id. Display only: the id is unauthenticated, so
    /// trust comes from the handshake, never from this flag.
    pub paired: bool,
}

/// A device this one is paired with, listed even when it cannot be reached now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    /// The device's id; pass it to [`Command::Forget`].
    pub id: PeerId,
    /// Its name as last heard, or the start of its id if it has not been heard since this run began.
    pub name: String,
    /// What kind of device it is.
    pub kind: DeviceKind,
    /// Whether it is in the nearby list now.
    pub online: bool,
    /// Its OS, as it said over an authenticated link (kept across restarts, so known while it is
    /// away), else as it announced on the network; `None` if neither has been heard.
    pub os: Option<Os>,
}

/// Everything the UI shows, published by the controller after every change.
/// The UI renders it and holds no connection state of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    /// This device's name.
    pub name: String,
    /// This device's operating system.
    pub os: Os,
    /// Where other devices can reach this one.
    pub address: Option<SocketAddr>,
    /// The current connection lifecycle phase.
    pub phase: Phase,
    /// The screens of the shared Desk; empty unless connected.
    pub desk: Vec<ScreenView>,
    /// The device whose screen has the cursor; `None` when not connected.
    pub focus: Option<PeerId>,
    /// Whether the UI may let the user rearrange the screens. Always false in the open
    /// core; the Pro module sets it.
    pub can_arrange: bool,
    /// Whether unknown devices may currently start pairing with this one.
    pub pairing_open: bool,
    /// Other Overdesk computers on the network, sorted by name.
    pub nearby: Vec<NearbyDevice>,
    /// Every paired device, sorted by name, whether or not it is nearby or connected, so one
    /// that is switched off can still be forgotten ([`Command::Forget`]).
    pub paired: Vec<PairedDevice>,
    /// A problem worth telling the user that doesn't end the session; the UI shows it
    /// until dismissed.
    pub notice: Option<String>,
    /// Files another computer offers, waiting for the user's answer ([`Command::AcceptFiles`]).
    /// Only Pro sets it; the open controller never does.
    pub offer: Option<FileOffer>,
}

impl AppState {
    /// A fresh state for a device called `name` running `os`: idle, nothing nearby or paired.
    pub fn new(name: String, os: Os) -> Self {
        Self {
            name,
            os,
            address: None,
            phase: Phase::Idle,
            desk: Vec::new(),
            focus: None,
            can_arrange: false,
            pairing_open: false,
            nearby: Vec::new(),
            paired: Vec::new(),
            notice: None,
            offer: None,
        }
    }
}

/// Files a connected computer wants to send, shown to the user to accept or decline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOffer {
    /// The computer that offers them; an answer must name it.
    pub from: PeerId,
    /// The offer's id; an answer must name it, so a click on an older banner answers nothing.
    pub id: [u8; 16],
    /// The name of the computer that offers them.
    pub from_name: String,
    /// How many files.
    pub count: usize,
    /// Their total size in bytes.
    pub bytes: u64,
}

/// Where the peer's screen sits relative to ours (the dialer decides).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The peer's screen sits to the left of this device's screen.
    Left,
    /// The peer's screen sits to the right of this device's screen.
    Right,
}

impl Side {
    /// The word the side is saved as.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// The text was neither `left` nor `right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownSide;

impl std::str::FromStr for Side {
    type Err = UnknownSide;

    fn from_str(s: &str) -> Result<Self, UnknownSide> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            _ => Err(UnknownSide),
        }
    }
}

/// Which edge of another computer's screen a screen is placed against. Unlike [`Side`],
/// which a two-computer session needs, it can also be above or below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Left of the anchor's screen.
    Left,
    /// Right of the anchor's screen.
    Right,
    /// Above the anchor's screen.
    Above,
    /// Below the anchor's screen.
    Below,
}

/// What the UI asks the controller to do. Commands are the controller's only input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Dial another device and, once trusted, share a Desk with it.
    Connect {
        /// The other device's address.
        address: SocketAddr,
        /// Where the other device's screen sits relative to this one.
        side: Side,
        /// The device this must turn out to be. The UI sets it when the user picked a
        /// nearby device shown as paired: mDNS is unauthenticated, so the dialed peer's
        /// fingerprint must match this id *and* be pinned, or the link is dropped.
        /// `None` means a first pairing or a typed address; the code check protects those.
        expect: Option<PeerId>,
    },
    /// The user's answer to whatever pairing code is showing: `true` if both screens match.
    /// Prefer [`Command::Answer`], which cannot land on a newer code than the one answered.
    AnswerCode(bool),
    /// The user's answer to the pairing code `code`. Ignored if that code is no longer the
    /// one being asked about (a stale click must not confirm a newer pairing).
    Answer {
        /// The code the user was looking at.
        code: u32,
        /// `true` if both screens show it.
        matches: bool,
    },
    /// End the current session or attempt, or dismiss a result, and return to idle.
    Disconnect,
    /// Clears the [`AppState::notice`].
    DismissNotice,
    /// Ends the link with one computer and keeps the others (Pro). The open controller has
    /// one peer, so it treats this like [`Command::Disconnect`] when it names the current peer.
    DisconnectPeer(PeerId),
    /// Forgets a paired device: any session with it ends, it is unpinned (a remote device's
    /// pairing and its secret are deleted) and it is not reconnected until paired again. Naming
    /// a device that is not paired is ignored.
    Forget(PeerId),
    /// The PIN the user read off a remote device, answering [`Phase::EnterPin`]. Ignored
    /// when no PIN is being asked for; the open controller ignores it.
    SubmitPin(String),
    /// The user's answer to the [`AppState::offer`] with this `id`: `true` to receive the
    /// files. An answer naming any other offer is ignored. The open controller ignores it.
    AcceptFiles {
        /// The [`FileOffer::id`] shown to the user.
        id: [u8; 16],
        /// `true` to receive the files.
        accept: bool,
    },
    /// Opens (for [`PAIRING_WINDOW`]) or closes pairing for devices that dial in.
    AllowPairing(bool),
    /// Puts `peer`'s screen on `side` of `anchor`'s. Used by Pro to arrange 3+ computers;
    /// the open controller ignores it.
    Place {
        /// The device whose screen moves.
        peer: PeerId,
        /// The device whose screen it is placed against.
        anchor: PeerId,
        /// Which side of the anchor's screen it goes on.
        side: Placement,
    },
}

/// How long "Pair a device" stays open for incoming devices.
pub const PAIRING_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Why the controller could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The settings folder could not be opened, or the identity could not be loaded.
    #[error("could not open the settings folder: {0}")]
    Store(String),
    /// The listening socket could not be bound.
    #[error("could not listen for other devices: {0}")]
    Bind(String),
    /// The screen layout could not be set up (the monitors, or a saved arrangement that no longer fits).
    #[error("could not set up the screen layout: {0}")]
    Layout(String),
    /// Input capture or injection could not start (not allowed by the system, or the OS refused).
    #[cfg(any(target_os = "macos", windows, free_unix))]
    #[error("could not start keyboard and mouse sharing: {0}")]
    Input(#[from] crate::native::StartError),
}

/// Whether a Disconnect in `phase` ends a live attempt (and so means "do not redial"),
/// as opposed to dismissing a finished one.
pub fn ends_attempt(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Connecting { .. } | Phase::Pairing { .. } | Phase::EnterPin { .. } | Phase::Connected { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_side_round_trips_through_its_word() {
        for side in [Side::Left, Side::Right] {
            assert_eq!(side.as_str().parse(), Ok(side));
        }
        assert_eq!("up".parse::<Side>(), Err(UnknownSide));
    }

    #[test]
    fn only_a_live_attempt_counts_as_leaving() {
        let addr = SocketAddr::from(([10, 0, 0, 1], 1));
        assert!(ends_attempt(&Phase::Connecting { address: addr }));
        assert!(ends_attempt(&Phase::Pairing {
            peer: "x".into(),
            code: 1
        }));
        assert!(ends_attempt(&Phase::Connected {
            peers: vec![],
            mode: Mode::Local
        }));
        // Dismissing a finished attempt must not switch off auto-reconnect.
        assert!(!ends_attempt(&Phase::Idle));
        assert!(!ends_attempt(&Phase::Lost { peer: "x".into() }));
        assert!(!ends_attempt(&Phase::Error("x".into())));
    }
}
