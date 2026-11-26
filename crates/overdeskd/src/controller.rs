// SPDX-License-Identifier: AGPL-3.0-only
//! What the app runs: one long-lived task that listens, dials, pairs and runs sessions.
//! The UI sends [`Command`]s and renders the [`AppState`] it publishes; nothing else.

use std::net::SocketAddr;
use std::path::PathBuf;

use overdesk_core::engine::Mode;
use overdesk_net::session::Me;
use overdesk_proto::Os;
use tokio::sync::{mpsc, watch};

use crate::{Platform, Side};

pub struct Config {
    pub store_dir: PathBuf,
    pub listen: SocketAddr,
    pub me: Me,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Connecting {
        address: SocketAddr,
    },
    /// Show `code`; the user answers with [`Command::AnswerCode`].
    Pairing {
        peer: String,
        code: u32,
    },
    Connected {
        peer: String,
        peer_os: Os,
        mode: Mode,
    },
    Lost {
        peer: String,
    },
    Error(String),
}

/// One screen of the shared Desk, named for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenView {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub me: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    pub name: String,
    pub os: Os,
    /// Where other devices can reach this one.
    pub address: Option<SocketAddr>,
    pub phase: Phase,
    pub desk: Vec<ScreenView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Connect { address: SocketAddr, side: Side },
    AnswerCode(bool),
    Disconnect,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("could not open the settings folder: {0}")]
    Store(String),
    #[error("could not listen for other devices: {0}")]
    Bind(String),
}

/// Starts the controller. Returns its command sender and live state.
pub async fn spawn(
    _config: Config,
    _platform: impl Platform + Clone,
) -> Result<(mpsc::UnboundedSender<Command>, watch::Receiver<AppState>), StartError> {
    todo!()
}
