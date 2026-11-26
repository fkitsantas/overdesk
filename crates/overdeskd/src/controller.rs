// SPDX-License-Identifier: AGPL-3.0-only
//! What the app runs: one long-lived task that listens, dials, pairs and runs sessions.
//! The UI sends [`Command`]s and renders the [`AppState`] it publishes; nothing else.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use overdesk_core::engine::{Action, Mode};
use overdesk_net::session::{Me, SessionError, pair};
use overdesk_net::store::Store;
use overdesk_net::transport::{Link, NetError, Node};
use overdesk_proto::{Os, PeerId};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::{DaemonError, Platform, Session, Side, establish, run};

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

/// Everything a session task needs from the controller.
struct Shared<P> {
    state: watch::Sender<AppState>,
    /// Where the user's answer to the current pairing code goes.
    answer: Mutex<Option<oneshot::Sender<bool>>>,
    store: Store,
    me: Me,
    platform: P,
}

impl<P> Shared<P> {
    fn set(&self, phase: Phase, desk: Option<Vec<ScreenView>>) {
        self.state.send_modify(|s| {
            s.phase = phase;
            if let Some(d) = desk {
                s.desk = d;
            }
        });
    }
}

/// Starts the controller. Returns its command sender and live state.
pub async fn spawn<P: Platform + Clone + Sync>(
    config: Config,
    platform: P,
) -> Result<(mpsc::UnboundedSender<Command>, watch::Receiver<AppState>), StartError> {
    let store = Store::open(&config.store_dir).map_err(|e| StartError::Store(e.to_string()))?;
    let identity = store.identity().map_err(|e| StartError::Store(e.to_string()))?;
    let node = Node::bind(&identity, config.listen).map_err(|e| StartError::Bind(e.to_string()))?;
    let (state, state_rx) = watch::channel(AppState {
        name: config.me.name.clone(),
        os: config.me.os,
        address: node.local_addr().ok(),
        phase: Phase::Idle,
        desk: Vec::new(),
    });
    let shared = Arc::new(Shared {
        state,
        answer: Mutex::new(None),
        store,
        me: config.me,
        platform,
    });
    let (commands, rx) = mpsc::unbounded_channel();
    tokio::spawn(control(Arc::new(node), shared, rx));
    Ok((commands, state_rx))
}

async fn control<P: Platform + Clone + Sync>(
    node: Arc<Node>,
    shared: Arc<Shared<P>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    // shortcut: one session at a time (Free tier: 2 devices); Pro runs one per peer.
    let mut active: Option<JoinHandle<()>> = None;
    let busy = |a: &Option<JoinHandle<()>>| a.as_ref().is_some_and(|h| !h.is_finished());
    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None => break,
                Some(Command::Connect { address, side }) if !busy(&active) => {
                    shared.set(Phase::Connecting { address }, Some(Vec::new()));
                    // Dial inside the task: a slow peer must not block Disconnect.
                    let (node, shared) = (node.clone(), shared.clone());
                    active = Some(tokio::spawn(async move {
                        let link = node.connect(address).await;
                        session(shared, link, side).await;
                    }));
                }
                Some(Command::Connect { .. }) => {}
                Some(Command::AnswerCode(matches)) => {
                    if let Some(tx) = shared.answer.lock().expect("answer lock").take() {
                        let _ = tx.send(matches);
                    }
                }
                Some(Command::Disconnect) => {
                    if let Some(task) = active.take() {
                        task.abort();
                        // An aborted session cannot clean up after itself.
                        shared.platform.swallow(false);
                        shared.platform.inject(Action::ReleaseAll);
                    }
                    shared.set(Phase::Idle, Some(Vec::new()));
                }
            },
            Some(setup) = node.accept() => {
                if busy(&active) {
                    continue; // dropping the setup refuses the connection
                }
                let shared = shared.clone();
                active = Some(tokio::spawn(async move {
                    let link = setup.await;
                    // The listener's side is decided by the dialer's Desk.
                    session(shared, link, Side::Right).await;
                }));
            }
        }
    }
}

/// Pairs if needed, establishes the Desk, runs until the peer is lost.
async fn session<P: Platform + Clone + Sync>(shared: Arc<Shared<P>>, link: Result<Link, NetError>, side: Side) {
    let mut link = match link {
        Ok(l) => l,
        Err(e) => return shared.set(Phase::Error(format!("Couldn't connect: {e}")), None),
    };
    let peer_fp = link.peer_fingerprint();
    if !shared.store.pins().unwrap_or_default().contains(&peer_fp) {
        let who = link.remote_address().ip().to_string();
        let confirm = async |code: u32| {
            let (tx, rx) = oneshot::channel();
            *shared.answer.lock().expect("answer lock") = Some(tx);
            shared.set(
                Phase::Pairing {
                    peer: who.clone(),
                    code,
                },
                None,
            );
            rx.await.unwrap_or(false)
        };
        match pair(&mut link, confirm).await {
            Ok(fp) => {
                if let Err(e) = shared.store.pin(fp) {
                    return shared.set(Phase::Error(format!("Couldn't save the pairing: {e}")), None);
                }
            }
            Err(SessionError::Declined) => {
                return shared.set(
                    Phase::Error("Pairing was cancelled on one of the devices.".into()),
                    None,
                );
            }
            Err(e) => return shared.set(Phase::Error(format!("Pairing failed: {e}")), None),
        }
    }
    let pins = shared.store.pins().unwrap_or_default();
    let s = match establish(link, |fp| pins.contains(fp), &shared.me, side).await {
        Ok(s) => s,
        Err(DaemonError::Session(SessionError::Net(e))) => {
            return shared.set(Phase::Error(format!("Connection dropped: {e}")), None);
        }
        Err(e) => return shared.set(Phase::Error(format!("Couldn't set up the Desk: {e}")), None),
    };
    let (peer, peer_os) = (s.peer_name.clone(), s.peer_os);
    shared.set(
        Phase::Connected {
            peer: peer.clone(),
            peer_os,
            mode: Mode::Local,
        },
        Some(desk_view(&s, &shared.me.name)),
    );
    let watcher = shared.clone();
    let on_mode = move |mode: Mode| {
        watcher.state.send_modify(|st| {
            if let Phase::Connected { mode: m, .. } = &mut st.phase {
                *m = mode;
            }
        })
    };
    run(s, shared.platform.capture(), shared.platform.clone(), on_mode).await;
    shared.set(Phase::Lost { peer }, Some(Vec::new()));
}

fn desk_view(s: &Session, my_name: &str) -> Vec<ScreenView> {
    let name = |p: &PeerId| {
        if *p == s.me {
            my_name.to_string()
        } else {
            s.peer_name.clone()
        }
    };
    s.desk
        .screens
        .iter()
        .map(|d| ScreenView {
            name: name(&d.peer),
            x: d.x,
            y: d.y,
            w: d.w,
            h: d.h,
            me: d.peer == s.me,
        })
        .collect()
}
