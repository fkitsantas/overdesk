// SPDX-License-Identifier: AGPL-3.0-only
//! The controller's shared state: what every session task reads and writes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use overdesk_core::engine::Mode;
use overdesk_net::discovery::Nearby;
use overdesk_net::pairing::Fingerprint;
use overdesk_net::session::Me;
use overdesk_net::sync::Relock;
use overdesk_net::transport::{Closer, NetError};
use overdesk_proto::PeerId;
use tokio::sync::watch;
use tokio::time::Instant;

use super::model::{AppState, Phase, ScreenView};
use super::nearby::{nearby_list, paired_list, remember_names};
use super::persist::{forget_name, learn_name, save_names};
use super::trust::{PairingWindow, PinBook};
use crate::Admit;

/// Everything a session task needs from the controller.
pub(super) struct Shared<P> {
    /// The store folder, which also holds [`SIDES_FILE`](super::persist::SIDES_FILE).
    pub(super) dir: PathBuf,
    /// Closes the running session's connection with a deliberate "bye".
    pub(super) closer: Mutex<Option<Closer>>,
    /// Peers that disconnected on purpose; not redialed until they leave and reappear
    /// on the network, or the user connects manually.
    pub(super) left: Mutex<BTreeSet<PeerId>>,
    /// Earliest time an automatic dial to a peer may be retried after a failure.
    pub(super) retry_after: Mutex<BTreeMap<PeerId, Instant>>,
    /// Publishes [`AppState`] to the UI.
    pub(super) state: watch::Sender<AppState>,
    /// The pinned peers, and the trust generation.
    pub(super) book: PinBook,
    /// The name of each paired device, kept in [`NAMES_FILE`](super::persist::NAMES_FILE) so it survives a restart. The name
    /// from an authenticated `Hello` always wins; one heard on the network only fills a gap.
    pub(super) names: Mutex<BTreeMap<PeerId, String>>,
    /// When unknown devices may pair, and the answer to the code on screen.
    pub(super) window: PairingWindow,
    /// Which session may write state. Bumped on Disconnect and whenever a session starts,
    /// so a task that outlives its abort cannot overwrite a newer phase.
    pub(super) generation: AtomicU64,
    /// Devices heard on the network, by id.
    pub(super) found: Mutex<BTreeMap<PeerId, Nearby>>,
    /// Who this device is, as sent to peers.
    pub(super) me: Me,
    /// The embedder's hook that vets each peer, if any.
    pub(super) admit: Option<Arc<dyn Admit>>,
    /// Whether the user was already told that a small file could not be saved.
    pub(super) save_warned: AtomicBool,
    /// The OS adapter that captures and injects input.
    pub(super) platform: P,
}

impl<P> Shared<P> {
    /// Opens pairing until `until`, or closes it with `None`, and tells the UI.
    pub(super) fn set_pairing(&self, until: Option<tokio::time::Instant>) {
        self.window.set(until);
        self.state.send_if_modified(|s| {
            let changed = s.pairing_open != until.is_some();
            s.pairing_open = until.is_some();
            changed
        });
    }

    /// The pinned peers, re-read only when `pins.txt` changed. An unreadable file counts as
    /// no pins (nobody is trusted) and tells the user once, rather than failing silently.
    pub(super) fn pins(&self) -> BTreeSet<Fingerprint> {
        let (pins, unreadable) = self.book.load();
        if let Some(e) = unreadable {
            eprintln!("overdesk: pinned devices unreadable: {e}");
            self.notify("Couldn't read the list of paired computers, so none can connect.".into());
        }
        pins
    }

    /// Remembers `name` for `id` as heard from the peer itself over its authenticated link,
    /// replacing whatever was known, and keeps it across restarts.
    pub(super) fn learn_name(&self, id: PeerId, name: &str) {
        let learned = learn_name(&self.dir, &mut self.names.relock(), id, name);
        // A name that could not be saved is still known for this run.
        let changed = learned.as_ref().map_or(true, |changed| *changed);
        self.saved(learned.map(drop));
        if changed {
            self.refresh_nearby();
        }
    }

    /// Reports a failed save of a small file (a name, a side) to the log, and to the user once per
    /// run: what is lost is only remembered across a restart.
    pub(super) fn saved(&self, result: Result<(), NetError>) {
        if let Err(e) = result {
            eprintln!("overdesk: could not save a setting: {e}");
            if !self.save_warned.swap(true, Ordering::SeqCst) {
                self.notify(
                    "Couldn't save a setting. Overdesk keeps working, but may forget it after a restart.".into(),
                );
            }
        }
    }

    /// Unpairs `peer`: deletes its pin and name, and makes every session that is still setting
    /// up with it stale, so it cannot finish connecting with the old trust.
    pub(super) fn unpair(&self, peer: PeerId) {
        self.book.revoke();
        self.book
            .unpin_peer(peer, |e| self.notify(format!("Couldn't forget that computer: {e}")));
        self.saved(forget_name(&self.dir, &mut self.names.relock(), peer));
        self.refresh_nearby();
    }

    /// Shows `text` to the user until dismissed.
    pub(super) fn notify(&self, text: String) {
        self.state.send_if_modified(|s| {
            let changed = s.notice.as_ref() != Some(&text);
            s.notice = Some(text);
            changed
        });
    }

    /// Recomputes the published `nearby` list from what was heard and what is pinned.
    pub(super) fn refresh_nearby(&self) {
        let pins = self.pins();
        let list = nearby_list(&self.found.relock(), &pins);
        let paired = {
            let mut names = self.names.relock();
            if remember_names(&mut names, &list) {
                self.saved(save_names(&self.dir, &names));
            }
            paired_list(&pins, &list, &names)
        };
        self.state.send_if_modified(|s| {
            let changed = s.nearby != list || s.paired != paired;
            (s.nearby, s.paired) = (list, paired);
            changed
        });
    }

    /// Publishes a new phase and, if given, a new Desk view, unless `generation` is stale.
    pub(super) fn set(&self, generation: u64, phase: Phase, desk: Option<Vec<ScreenView>>) {
        self.state.send_if_modified(|s| {
            // Checked under the state lock, so it is atomic with `begin`.
            if self.generation.load(Ordering::SeqCst) != generation {
                return false;
            }
            let mut changed = s.phase != phase;
            s.phase = phase;
            if let Some(d) = desk {
                changed |= s.desk != d;
                s.desk = d;
            }
            let at = focus(&s.phase, &s.desk);
            changed |= s.focus != at;
            s.focus = at;
            changed
        });
    }

    /// Invalidates every running session, publishes `phase` with an empty Desk, and
    /// returns the new generation for the session (if any) that follows.
    pub(super) fn begin(&self, phase: Phase) -> u64 {
        let mut generation = 0;
        self.state.send_if_modified(|s| {
            generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            let changed = s.phase != phase || !s.desk.is_empty() || s.focus.is_some();
            s.phase = phase;
            s.desk = Vec::new();
            s.focus = None;
            changed
        });
        generation
    }
}

/// Which device's screen has the cursor: the peer being driven, else this device's own
/// (it is local or is being driven). `None` unless connected.
pub(super) fn focus(phase: &Phase, desk: &[ScreenView]) -> Option<PeerId> {
    match phase {
        Phase::Connected {
            mode: Mode::Driving(p), ..
        } => Some(*p),
        Phase::Connected { .. } => desk.iter().find(|d| d.me).map(|d| d.id),
        _ => None,
    }
}
