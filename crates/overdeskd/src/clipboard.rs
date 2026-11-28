// SPDX-License-Identifier: AGPL-3.0-only
//! Clipboard sharing: which local copies go to the peer, how a read is made safe against a
//! private copy, and the per-session [`ClipboardPump`] that watches, reads and writes it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::Clipboard;
use crate::task::AbortOnDrop;

/// How often the clipboard is checked where the OS gives no change notification but a change
/// counter (macOS `changeCount`): a tick reads only the counter, contents when it moves.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);

/// The same where there is no counter either (Wayland): every tick reads the clipboard
/// through the portal or data-control, so it is slower.
const CLIPBOARD_POLL_BLIND: Duration = Duration::from_secs(1);

/// One clipboard read, or `None` to skip it. The read is trusted only if the copy was not
/// private before or after it, and nothing was copied during it: the change tokens match, or
/// without tokens `second`, an immediate re-read, equals `first`. This closes the window where
/// a password manager copies a secret between the privacy check and the read.
fn trusted_read<T: PartialEq>(
    private: (bool, bool),
    tokens: (Option<i64>, Option<i64>),
    first: T,
    second: Option<&T>,
) -> Option<T> {
    let stable = match tokens {
        (Some(before), Some(after)) => before == after,
        _ => second == Some(&first),
    };
    (!private.0 && !private.1 && stable).then_some(first)
}

/// Runs `read` on the clipboard only when the copy is not private, guarding against a copy
/// landing mid-read: privacy, read, privacy again, with a change token (or a second read where
/// the OS has none). `None` means skip this time. Blocks. Every outbound clipboard read, text,
/// image or files, must go through this, in the open daemon and in the Pro hub alike.
pub fn read_checked<P: Clipboard + ?Sized, T: PartialEq>(p: &P, read: impl Fn(&P) -> T) -> Option<T> {
    let token_before = p.clipboard_change_count();
    if p.clipboard_is_private() {
        return None;
    }
    let first = read(p);
    let token_after = p.clipboard_change_count();
    let second = (token_before.is_none() || token_after.is_none()).then(|| read(p));
    trusted_read(
        (false, p.clipboard_is_private()),
        (token_before, token_after),
        first,
        second.as_ref(),
    )
}

/// Whether a poll tick must read the clipboard: when the OS has no change counter, or the
/// counter moved since the last read. Remembers `now` as the last counter seen.
pub fn clipboard_moved(last: &mut Option<i64>, now: Option<i64>) -> bool {
    let moved = now.is_none() || now != *last;
    *last = now;
    moved
}
/// Room kept below `MAX_FRAME_LEN` for the message tag and the text's length prefix.
const CLIPBOARD_FRAME_OVERHEAD: usize = 16;

/// Decides which local clipboard changes go to the peer.
/// The free tier shares plain text only; images and files are Pro.
pub struct ClipboardSync {
    /// The text seen on the local clipboard at the last check.
    seen: Option<String>,
    /// The text the peer sent that has not yet been replaced locally; never echoed back.
    remote: Option<String>,
    /// Whether the last copy was too large to share and has not been reported yet.
    too_large: bool,
    /// Whether the starting clipboard could not be read cleanly (private, changing, unreadable):
    /// the first clean text read then becomes the baseline instead of being sent.
    baseline_unknown: bool,
}

impl ClipboardSync {
    /// Starts from `initial`, the clipboard as it was before the session: whatever was
    /// copied earlier is private until the user copies something during the session.
    pub fn new(initial: Option<String>) -> Self {
        Self {
            seen: initial,
            remote: None,
            too_large: false,
            baseline_unknown: false,
        }
    }

    /// Starts when the clipboard could not be read cleanly. The copy sitting there may be a
    /// secret, so the first text read becomes the baseline and is never sent.
    pub fn unknown() -> Self {
        Self {
            baseline_unknown: true,
            ..Self::new(None)
        }
    }

    /// Feeds one local reading; returns the text to send if it is a new, sendable copy.
    pub fn local_changed(&mut self, text: Option<String>) -> Option<String> {
        if self.baseline_unknown {
            // `None` cannot tell "empty" from "skipped as private", so keep waiting for text.
            self.baseline_unknown = text.is_none();
            self.seen = text;
            return None;
        }
        if text == self.seen {
            return None;
        }
        self.seen = text.clone();
        let echo = self.remote.take() == text;
        let sendable = text.filter(|t| !echo && !t.is_empty());
        self.too_large = sendable.as_ref().is_some_and(|t| !fits_frame(t));
        sendable.filter(|t| fits_frame(t))
    }

    /// Whether the copy just fed to [`ClipboardSync::local_changed`] was refused for its size;
    /// true once per such copy, so the user can be told instead of left wondering.
    pub fn take_too_large(&mut self) -> bool {
        std::mem::take(&mut self.too_large)
    }

    /// Records text the peer sent, so the change it causes here is not sent back.
    pub fn received(&mut self, text: &str) {
        self.remote = Some(text.to_owned());
    }
}

/// Whether clipboard `text` fits one frame.
fn fits_frame(text: &str) -> bool {
    text.len() + CLIPBOARD_FRAME_OVERHEAD <= overdesk_proto::MAX_FRAME_LEN
}

/// What the user is told when a copy is too large to share.
const CLIPBOARD_TOO_LARGE: &str = "Clipboard text too large to share (over 1 MB)";

/// One checked clipboard read on the blocking pool: the OS may hold the clipboard or the text
/// may be large, and a slow read must never delay injecting input.
async fn read_text<P: Clipboard + Sync>(p: Arc<P>) -> Option<String> {
    read_clean(p).await.flatten()
}

/// [`read_text`], but `None` when the read was not clean (private, changing, failed), as
/// opposed to `Some(None)` for a clean read of no text.
async fn read_clean<P: Clipboard + Sync>(p: Arc<P>) -> Option<Option<String>> {
    tokio::task::spawn_blocking(move || read_checked(&*p, |q: &P| q.clipboard_text()))
        .await
        .unwrap_or(None)
}

/// Sends a reading of the local clipboard to `readings` whenever it may have changed: on the
/// watcher's notification (`watch` is held here so it stops with the task), else by polling.
async fn watch_clipboard<P: Clipboard + Sync>(
    platform: Arc<P>,
    watch: Option<Box<dyn Send>>,
    mut wake: UnboundedReceiver<()>,
    readings: UnboundedSender<Option<String>>,
) {
    if watch.is_some() {
        while wake.recv().await.is_some() {
            while wake.try_recv().is_ok() {} // one read covers a burst of notifications
            if readings.send(read_text(platform.clone()).await).is_err() {
                break;
            }
        }
        return;
    }
    // A change counter makes a tick nearly free; without one every tick reads the clipboard.
    let every = if platform.clipboard_change_count().is_some() {
        CLIPBOARD_POLL
    } else {
        CLIPBOARD_POLL_BLIND
    };
    let mut last = None;
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    loop {
        tick.tick().await;
        if !clipboard_moved(&mut last, platform.clipboard_change_count()) {
            continue;
        }
        if readings.send(read_text(platform.clone()).await).is_err() {
            break;
        }
    }
}

/// One writer applies the peer's clipboard text in order, off the async threads; only the
/// newest of a backlog is applied. It ends when the returned sender drops.
fn spawn_clipboard_writer<P: Clipboard + Sync>(platform: Arc<P>) -> UnboundedSender<String> {
    let (writes, mut pending) = unbounded_channel::<String>();
    drop(tokio::task::spawn_blocking(move || {
        while let Some(mut text) = pending.blocking_recv() {
            while let Ok(newer) = pending.try_recv() {
                text = newer;
            }
            platform.set_clipboard_text(&text);
        }
    }));
    writes
}

/// The clipboard half of a running session: a watcher feeding readings in, a writer applying
/// the peer's text, and the [`ClipboardSync`] that decides what is worth sending.
pub(crate) struct ClipboardPump {
    /// Applies the peer's text off the async threads; ends when this drops.
    writes: UnboundedSender<String>,
    /// Watches or polls the local clipboard; aborted on drop.
    _poll: AbortOnDrop,
    /// Readings of the local clipboard, from [`watch_clipboard`].
    readings: UnboundedReceiver<Option<String>>,
    /// Decides which readings go to the peer.
    sync: ClipboardSync,
}

impl ClipboardPump {
    /// Starts watching the clipboard of `platform`, with its current contents as the baseline
    /// (never sent).
    pub(crate) async fn start<P: Clipboard + Sync>(platform: Arc<P>) -> Self {
        // Watch before the first read, so a copy landing between the two is not missed.
        let (wake_tx, wake) = unbounded_channel::<()>();
        let watch = platform.watch_clipboard(Box::new(move || {
            let _ = wake_tx.send(());
        }));
        let sync = read_clean(platform.clone())
            .await
            .map_or_else(ClipboardSync::unknown, ClipboardSync::new);
        let (readings_tx, readings) = unbounded_channel::<Option<String>>();
        let poll = AbortOnDrop(tokio::spawn(watch_clipboard(
            platform.clone(),
            watch,
            wake,
            readings_tx,
        )));
        let writes = spawn_clipboard_writer(platform);
        Self {
            writes,
            _poll: poll,
            readings,
            sync,
        }
    }

    /// The next reading of the local clipboard; `None` once the watcher is gone. Cancel-safe.
    pub(crate) async fn next_reading(&mut self) -> Option<Option<String>> {
        self.readings.recv().await
    }

    /// Feeds one reading. Returns the text to send to the peer if it is a new, sendable copy;
    /// `on_notice` hears when a copy is too large to share.
    pub(crate) fn on_reading(&mut self, now: Option<String>, on_notice: &mut impl FnMut(String)) -> Option<String> {
        let to_send = self.sync.local_changed(now);
        if self.sync.take_too_large() {
            on_notice(CLIPBOARD_TOO_LARGE.into());
        }
        to_send
    }

    /// Applies text the peer sent, without echoing it back.
    pub(crate) fn received(&mut self, text: String) {
        self.sync.received(&text);
        let _ = self.writes.send(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(s: &str) -> Option<String> {
        Some(s.into())
    }

    #[test]
    fn a_read_is_trusted_only_when_clean_and_unchanged() {
        let t = || some("secret?");
        // Tokens match and neither check was private: sent.
        assert_eq!(trusted_read((false, false), (Some(1), Some(1)), t(), None), Some(t()));
        // Private before or after the read: skipped.
        assert_eq!(trusted_read((true, false), (Some(1), Some(1)), t(), None), None);
        assert_eq!(trusted_read((false, true), (Some(1), Some(1)), t(), None), None);
        // Something was copied during the read: skipped.
        assert_eq!(trusted_read((false, false), (Some(1), Some(2)), t(), None), None);
        // No tokens: the immediate re-read must agree.
        assert_eq!(trusted_read((false, false), (None, None), t(), Some(&t())), Some(t()));
        assert_eq!(
            trusted_read((false, false), (None, None), t(), Some(&some("other"))),
            None
        );
        assert_eq!(trusted_read((false, false), (None, None), t(), None), None);
    }

    #[test]
    fn polling_reads_only_when_the_counter_moves() {
        let mut last = None;
        assert!(clipboard_moved(&mut last, Some(7)));
        assert!(!clipboard_moved(&mut last, Some(7)));
        assert!(clipboard_moved(&mut last, Some(8)));
        // No counter: always read.
        assert!(clipboard_moved(&mut last, None));
        assert!(clipboard_moved(&mut last, None));
    }

    #[test]
    fn existing_clipboard_is_not_sent_but_later_copies_are() {
        let mut c = ClipboardSync::new(some("old"));
        assert_eq!(c.local_changed(some("old")), None);
        assert_eq!(c.local_changed(some("new")), some("new"));
        assert_eq!(c.local_changed(some("new")), None, "unchanged text is sent once");
    }

    #[test]
    fn an_unreadable_start_takes_the_first_text_as_baseline_without_sending_it() {
        let mut c = ClipboardSync::unknown();
        assert_eq!(c.local_changed(None), None, "a skipped read decides nothing");
        assert_eq!(c.local_changed(some("secret")), None, "baseline, never sent");
        assert_eq!(c.local_changed(some("secret")), None);
        assert_eq!(c.local_changed(some("new")), some("new"));
    }

    #[test]
    fn peer_text_is_not_echoed_but_a_later_copy_of_it_is() {
        let mut c = ClipboardSync::new(None);
        c.received("from peer");
        assert_eq!(c.local_changed(some("from peer")), None);
        assert_eq!(c.local_changed(some("mine")), some("mine"));
        assert_eq!(c.local_changed(some("from peer")), some("from peer"));
    }

    #[test]
    fn failed_write_does_not_send_the_old_text() {
        let mut c = ClipboardSync::new(some("old"));
        c.received("from peer");
        assert_eq!(c.local_changed(some("old")), None);
    }

    #[test]
    fn empty_and_oversized_text_is_skipped() {
        let mut c = ClipboardSync::new(None);
        assert_eq!(c.local_changed(some("")), None);
        assert_eq!(c.local_changed(None), None);
        let max = overdesk_proto::MAX_FRAME_LEN - CLIPBOARD_FRAME_OVERHEAD;
        assert_eq!(c.local_changed(Some("x".repeat(max + 1))), None);
        assert!(c.local_changed(Some("y".repeat(max))).is_some());
    }

    #[test]
    fn an_oversized_copy_is_reported_once() {
        let mut c = ClipboardSync::new(None);
        let max = overdesk_proto::MAX_FRAME_LEN - CLIPBOARD_FRAME_OVERHEAD;
        assert_eq!(c.local_changed(Some("x".repeat(max + 1))), None);
        assert!(c.take_too_large());
        assert!(!c.take_too_large(), "reported once per copy");
        // The same text read again is not a new copy.
        assert_eq!(c.local_changed(Some("x".repeat(max + 1))), None);
        assert!(!c.take_too_large());
        // A later small copy clears nothing to report.
        assert!(c.local_changed(some("ok")).is_some());
        assert!(!c.take_too_large());
    }
}
