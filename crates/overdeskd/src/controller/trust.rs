// SPDX-License-Identifier: AGPL-3.0-only
//! Who this device trusts: the pinned peers ([`PinBook`]), when and how often strangers may pair
//! ([`PairingWindow`]), the check that a dial reached the peer it aimed at ([`check_expected`]) and
//! the pairing prompt itself ([`pair_with_user`]). Shared by the open controller and the Pro one, so
//! the two cannot drift on what makes a peer trusted.
//!
//! Locks here survive a panic elsewhere: the guarded data is plain bookkeeping that is valid after
//! any single update, so a poisoned lock is no reason to take the controller down.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use overdesk_net::pairing::{Fingerprint, peer_id};
use overdesk_net::session::{SessionError, pair};
use overdesk_net::store::Store;
use overdesk_net::sync::Relock;
use overdesk_net::transport::{CloseReason, Link, NetError};
use overdesk_proto::{PeerId, log};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::model::Phase;

/// Failed inbound pairings before further attempts are refused for a while.
const FREE_PAIRING_FAILURES: u32 = 3;

/// How long inbound pairing is refused after `failures` consecutive failures:
/// nothing for the first few, then 10 s, 30 s and a 60 s cap.
fn pairing_backoff(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0..FREE_PAIRING_FAILURES => 0,
        FREE_PAIRING_FAILURES => 10,
        4 => 30,
        _ => 60,
    })
}

/// Consecutive failed inbound pairings and the time before which none is accepted.
/// Global on purpose: an attacker can change address.
#[derive(Default)]
pub(super) struct Backoff {
    /// Failures since the last successful pairing.
    failures: u32,
    /// Inbound pairing is refused until then.
    until: Option<Instant>,
}

impl Backoff {
    /// Whether inbound pairing is refused at `now`.
    pub(super) fn blocked(&self, now: Instant) -> bool {
        self.until.is_some_and(|t| now < t)
    }

    /// Counts a failed attempt and starts the delay it has earned.
    pub(super) fn fail(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.until = Some(now + pairing_backoff(self.failures));
    }
}

/// The pinned fingerprints read from `pins.txt`, valid while the file's stamp is unchanged.
#[derive(Default)]
struct PinCache {
    /// Modified time and length of `pins.txt` when read; `None` when it did not exist.
    stamp: Option<(SystemTime, u64)>,
    /// Whether `pins` holds a real reading.
    valid: bool,
    /// The fingerprints read.
    pins: BTreeSet<Fingerprint>,
}

/// The pinned peers of one store folder: a cached view of `pins.txt`, pinning and unpinning, and
/// the trust generation that makes a session started before a Forget stale.
pub struct PinBook {
    /// The store folder, where `pins.txt` lives.
    dir: PathBuf,
    /// This device's identity and pinned peers, shared with whoever else reads the same store.
    store: Arc<Store>,
    /// The pins as last read, with the stamp of the file they came from.
    cache: Mutex<PinCache>,
    /// Whether the file is known to be unreadable, so that is reported once per outage.
    failed: AtomicBool,
    /// Bumped by every [`revoke`](Self::revoke).
    trust: AtomicU64,
}

impl PinBook {
    /// The pins of `store`, whose folder is `dir`.
    pub fn new(dir: PathBuf, store: Arc<Store>) -> Self {
        Self {
            dir,
            store,
            cache: Mutex::default(),
            failed: AtomicBool::new(false),
            trust: AtomicU64::new(0),
        }
    }

    /// The pinned peers, re-read only when `pins.txt` changed, and, as the second value, the error
    /// text the first time the file is found unreadable (until it reads again). An unreadable
    /// file counts as no pins: nobody is trusted until it is readable again.
    pub fn load(&self) -> (BTreeSet<Fingerprint>, Option<String>) {
        let stamp = std::fs::metadata(self.dir.join("pins.txt"))
            .and_then(|m| Ok((m.modified()?, m.len())))
            .ok();
        let mut cache = self.cache.relock();
        if cache.valid && cache.stamp == stamp {
            return (cache.pins.clone(), None);
        }
        match self.store.pins() {
            Ok(pins) => {
                *cache = PinCache {
                    stamp,
                    valid: true,
                    pins: pins.clone(),
                };
                self.failed.store(false, Ordering::SeqCst);
                (pins, None)
            }
            Err(e) => {
                cache.valid = false;
                let first = !self.failed.swap(true, Ordering::SeqCst);
                (BTreeSet::new(), first.then(|| e.to_string()))
            }
        }
    }

    /// Pins `fp`.
    ///
    /// # Errors
    ///
    /// [`NetError`] if the pins cannot be read or saved.
    pub fn pin(&self, fp: Fingerprint) -> Result<(), NetError> {
        self.store.pin(fp)
    }

    /// Unpins every fingerprint of `peer`, reporting each failure to `on_error`. Returns whether
    /// the peer was pinned at all. Reads the file itself, not the cache, so a Forget never acts on
    /// a stale view.
    pub fn unpin_peer(&self, peer: PeerId, mut on_error: impl FnMut(String)) -> bool {
        let pins = match self.store.pins() {
            Ok(p) => p,
            Err(e) => {
                on_error(e.to_string());
                return false;
            }
        };
        let mut found = false;
        for fp in pins.into_iter().filter(|fp| peer_id(fp) == peer) {
            found = true;
            if let Err(e) = self.store.unpin(fp) {
                on_error(e.to_string());
            }
        }
        found
    }

    /// Makes every session that started before now stale: it may not finish connecting with the
    /// trust it began with. Called by every Forget.
    pub fn revoke(&self) {
        self.trust.fetch_add(1, Ordering::SeqCst);
    }

    /// The trust generation to remember when a session starts, for [`still_trusted`](Self::still_trusted).
    pub fn generation(&self) -> u64 {
        self.trust.load(Ordering::SeqCst)
    }

    /// Whether a session that began at `generation` may still connect to the peer `fp`: nothing
    /// was forgotten since, and `fp` is still pinned.
    pub fn still_trusted(&self, generation: u64, fp: &Fingerprint) -> bool {
        self.generation() == generation && self.load().0.contains(fp)
    }
}

/// A dial aimed at one device reached another, or one that is not paired. Its text is the
/// sentence for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnexpectedPeer;

impl std::fmt::Display for UnexpectedPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This isn't the computer you paired with. Pair it again if it was reset.")
    }
}

/// Checks that a dial with `expect` set reached a pinned peer with that id. The caller closes the
/// link and never offers to pair, since someone else answered at the announced address.
///
/// # Errors
///
/// [`UnexpectedPeer`] if `peer` is not `expect` or is not in `pins`.
pub fn check_expected(
    expect: Option<PeerId>,
    peer: &Fingerprint,
    pins: &BTreeSet<Fingerprint>,
) -> Result<(), UnexpectedPeer> {
    match expect {
        Some(id) if peer_id(peer) != id || !pins.contains(peer) => Err(UnexpectedPeer),
        _ => Ok(()),
    }
}

/// Where the user's answer to the pairing code on screen goes.
type Answer = (u32, oneshot::Sender<bool>);

/// When strangers may pair: the window the user opens, the backoff after failed attempts, the one
/// pairing at a time (one code on screen) and the answer slot of that code's prompt.
#[derive(Default)]
pub struct PairingWindow {
    /// Until when unknown devices dialing in may pair. Closed by default.
    until: Mutex<Option<Instant>>,
    /// Failed inbound pairings, which stall further ones. Survives the window closing.
    backoff: Mutex<Backoff>,
    /// Whether a pairing is in progress.
    busy: AtomicBool,
    /// The current pairing code and where the user's answer to it goes.
    answer: Mutex<Option<Answer>>,
}

/// The one pairing in progress; dropping it, however the pairing ends (an abort included), lets the next start.
pub struct PairingSlot<'a>(&'a AtomicBool);

impl Drop for PairingSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl PairingWindow {
    /// Whether an unknown device dialing in may start pairing right now.
    pub fn allowed(&self) -> bool {
        let now = Instant::now();
        let open = self.until.relock().is_some_and(|t| now < t);
        open && !self.backoff.relock().blocked(now)
    }

    /// Opens the window until `until`, or closes it with `None`.
    pub fn set(&self, until: Option<Instant>) {
        *self.until.relock() = until;
    }

    /// Whether the window is still the one that closes at `deadline`, that is, nobody re-armed
    /// or closed it since.
    pub fn closes_at(&self, deadline: Instant) -> bool {
        *self.until.relock() == Some(deadline)
    }

    /// Claims the one pairing at a time, or `None` if another is in progress.
    pub fn claim(&self) -> Option<PairingSlot<'_>> {
        (!self.busy.swap(true, Ordering::SeqCst)).then_some(PairingSlot(&self.busy))
    }

    /// Shows `code` to be answered: the receiver yields the user's answer (`matches`).
    pub fn prompt(&self, code: u32) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        *self.answer.relock() = Some((code, tx));
        rx
    }

    /// Delivers the user's answer to the pairing code on screen. With `code` set, only to that
    /// code: a click on an older prompt is ignored.
    pub fn answer(&self, code: Option<u32>, matches: bool) {
        let mut slot = self.answer.relock();
        if code.is_none_or(|c| slot.as_ref().is_some_and(|(shown, _)| *shown == c))
            && let Some((_, tx)) = slot.take()
        {
            let _ = tx.send(matches);
        }
    }

    /// Records how an inbound pairing ended: a success clears the backoff, a failure extends it.
    pub fn record(&self, paired: bool) {
        let mut b = self.backoff.relock();
        if paired {
            *b = Backoff::default();
        } else {
            b.fail(Instant::now());
        }
    }

    /// Forgets the prompt in progress, if any (its dial was aborted).
    pub fn drop_prompt(&self) {
        *self.answer.relock() = None;
    }
}

/// Which side opened the connection a pairing runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// This device dialed the other.
    Outbound,
    /// The other device dialed this one.
    Inbound,
}

/// Why [`pair_with_user`] did not pair.
#[derive(Debug, PartialEq, Eq)]
pub enum Unpaired {
    /// Turned away without a prompt: pairing was closed, or another pairing is in progress.
    /// Nothing for a stranger to probe, and nothing to tell the user.
    Refused,
    /// The user declined or it failed: this is what to tell them.
    Failed(String),
}

/// Pairs with the device on `link` after the user confirms the code. An [`Inbound`](Direction::Inbound) stranger may
/// pair only while the window is open, and only one pairing runs at a time. `show` puts a phase on
/// screen (the code); `on_paired` runs as soon as the pairing succeeded, before it is saved (the
/// inbound window closes there, but only if this pairing used it, so the caller checks the direction).
///
/// # Errors
///
/// [`Unpaired`] when the attempt is over without a pairing.
pub async fn pair_with_user(
    link: &mut Link,
    direction: Direction,
    book: &PinBook,
    window: &PairingWindow,
    show: impl Fn(Phase),
    on_paired: impl FnOnce(),
) -> Result<(), Unpaired> {
    let inbound = direction == Direction::Inbound;
    if inbound && !window.allowed() {
        // Deny by default: no prompt, nothing for a stranger to probe.
        link.close(CloseReason::NotPaired);
        return Err(Unpaired::Refused);
    }
    let Some(_slot) = window.claim() else {
        link.close(CloseReason::Busy);
        return Err(Unpaired::Refused);
    };
    let who = link.remote_address().ip().to_string();
    // The code on the screens is never logged: it is what proves both people see the same device.
    log::info(format_args!("pairing started with {who} ({direction:?})"));
    let confirm = async |code: u32| {
        let rx = window.prompt(code);
        show(Phase::Pairing {
            peer: who.clone(),
            code,
        });
        rx.await.unwrap_or(false)
    };
    let paired = pair(link, confirm).await;
    if inbound {
        window.record(paired.is_ok());
    }
    log::info(format_args!(
        "pairing {}",
        match &paired {
            Ok(_) => "succeeded".to_owned(),
            Err(SessionError::Declined) => "declined".to_owned(),
            Err(e) => format!("failed: {e}"),
        }
    ));
    match paired {
        Ok(fp) => {
            on_paired();
            book.pin(fp)
                .map_err(|e| Unpaired::Failed(format!("Couldn't save the pairing: {e}")))
        }
        Err(SessionError::Declined) => Err(Unpaired::Failed("Pairing was cancelled on one of the devices.".into())),
        Err(e) => Err(Unpaired::Failed(format!("Pairing failed: {e}"))),
    }
}

/// How often a paired device announced at a new address may skip its dial backoff. Announcements
/// are unauthenticated, so without this limit anyone on the network could keep the backoff from
/// ever running by announcing the device at changing addresses.
pub const MOVE_FORGIVE_EVERY: Duration = Duration::from_secs(60);

/// Drops `id`'s dial backoff in `retry` because it was announced at a new address, at most once per
/// [`MOVE_FORGIVE_EVERY`] (tracked in `forgiven`). Returns whether the backoff was dropped.
pub fn forgive_move(
    retry: &mut BTreeMap<PeerId, Instant>,
    forgiven: &mut BTreeMap<PeerId, Instant>,
    id: PeerId,
    now: Instant,
) -> bool {
    if forgiven.get(&id).is_some_and(|t| now < *t + MOVE_FORGIVE_EVERY) {
        return false;
    }
    forgiven.insert(id, now);
    // Old entries only matter for one window; keep the map as small as the peers that moved.
    forgiven.retain(|_, t| now < *t + MOVE_FORGIVE_EVERY);
    retry.remove(&id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let secs = |n| pairing_backoff(n).as_secs();
        assert_eq!([0, 1, 2, 3, 4, 5, 99].map(secs), [0, 0, 0, 10, 30, 60, 60]);
    }

    #[test]
    fn backoff_blocks_only_until_its_deadline() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        for _ in 0..2 {
            b.fail(t0);
        }
        assert!(!b.blocked(t0), "the first failures are free");
        b.fail(t0);
        assert!(b.blocked(t0 + Duration::from_secs(9)));
        assert!(!b.blocked(t0 + Duration::from_secs(10)));
        b.fail(t0);
        assert!(b.blocked(t0 + Duration::from_secs(29)));
    }

    /// A pin book on a fresh folder.
    fn book(name: &str) -> (PinBook, PathBuf) {
        let dir = std::env::temp_dir().join(format!("overdesk-trust-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).expect("store");
        (PinBook::new(dir.clone(), Arc::new(store)), dir)
    }

    /// A fingerprint whose peer id is `n` repeated.
    fn fp(n: u8) -> Fingerprint {
        [n; 32]
    }

    #[test]
    fn pins_follow_the_file_and_forget_revokes() {
        let (book, dir) = book("pins");
        assert!(book.load().0.is_empty());
        let before = book.generation();
        book.pin(fp(1)).unwrap();
        book.pin(fp(2)).unwrap();
        assert_eq!(book.load(), (BTreeSet::from([fp(1), fp(2)]), None));
        assert!(book.still_trusted(before, &fp(1)));
        assert!(!book.still_trusted(before, &fp(3)));
        let mut errors = Vec::new();
        assert!(book.unpin_peer(peer_id(&fp(1)), |e| errors.push(e)));
        assert!(!book.unpin_peer(peer_id(&fp(1)), |e| errors.push(e)), "already gone");
        assert!(errors.is_empty());
        assert_eq!(book.load().0, BTreeSet::from([fp(2)]));
        book.revoke();
        assert!(
            !book.still_trusted(before, &fp(2)),
            "a Forget made the old session stale"
        );
        assert!(book.still_trusted(book.generation(), &fp(2)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_pins_are_reported_once_and_trust_nobody() {
        let (book, dir) = book("bad");
        book.pin(fp(1)).unwrap();
        assert!(book.load().1.is_none());
        std::fs::write(dir.join("pins.txt"), "this is not a pin\n").unwrap();
        let (pins, first) = book.load();
        assert!(pins.is_empty());
        assert!(first.is_some(), "the first failure is reported");
        assert_eq!(book.load(), (BTreeSet::new(), None), "and not again");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_expected_pinned_peer_passes() {
        let pins = BTreeSet::from([fp(1)]);
        assert_eq!(check_expected(None, &fp(9), &pins), Ok(()));
        assert_eq!(check_expected(Some(peer_id(&fp(1))), &fp(1), &pins), Ok(()));
        assert_eq!(
            check_expected(Some(peer_id(&fp(2))), &fp(1), &pins),
            Err(UnexpectedPeer)
        );
        assert_eq!(
            check_expected(Some(peer_id(&fp(2))), &fp(2), &pins),
            Err(UnexpectedPeer)
        );
    }

    #[test]
    fn the_window_is_closed_until_opened_and_backs_off() {
        let w = PairingWindow::default();
        assert!(!w.allowed());
        let slot = w.claim().expect("free");
        assert!(w.claim().is_none(), "one pairing at a time");
        drop(slot);
        assert!(w.claim().is_some(), "free again once it ended");
        let deadline = Instant::now() + Duration::from_secs(60);
        w.set(Some(deadline));
        assert!(w.allowed() && w.closes_at(deadline));
        w.set(Some(deadline + Duration::from_secs(1)));
        assert!(!w.closes_at(deadline), "re-armed");
        for _ in 0..3 {
            w.backoff.relock().fail(Instant::now());
        }
        assert!(!w.allowed(), "three failures stall it");
        w.set(None);
        assert!(!w.allowed());
    }

    #[test]
    fn an_answer_reaches_only_the_code_it_names() {
        let w = PairingWindow::default();
        let mut rx = w.prompt(7);
        w.answer(Some(8), true);
        assert!(rx.try_recv().is_err(), "a click on another code is ignored");
        w.answer(Some(7), true);
        assert_eq!(rx.try_recv(), Ok(true));
        let mut rx = w.prompt(7);
        w.answer(None, false);
        assert_eq!(rx.try_recv(), Ok(false), "an unnamed answer takes whatever is showing");
    }

    #[test]
    fn a_moved_device_skips_its_backoff_at_most_once_a_minute() {
        let (id, now) = (PeerId([7; 16]), Instant::now());
        let (mut retry, mut forgiven) = (BTreeMap::new(), BTreeMap::new());
        retry.insert(id, now + Duration::from_secs(60));
        assert!(forgive_move(&mut retry, &mut forgiven, id, now));
        assert!(retry.is_empty());
        // A spoofed announcement at yet another address cannot clear the next backoff early.
        retry.insert(id, now + Duration::from_secs(60));
        assert!(!forgive_move(
            &mut retry,
            &mut forgiven,
            id,
            now + Duration::from_secs(10)
        ));
        assert!(retry.contains_key(&id));
        assert!(forgive_move(&mut retry, &mut forgiven, id, now + MOVE_FORGIVE_EVERY));
        assert!(retry.is_empty());
    }
}
