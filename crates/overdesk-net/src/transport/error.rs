// SPDX-License-Identifier: AGPL-3.0-only
//! What can go wrong on a link, and the reasons a device gives when it closes one on purpose.

use quinn::{Connection, ConnectionError};

use overdesk_proto::FrameError;

/// Errors raised by the transport layer. Every failure that can be caused by the peer
/// maps to a variant here rather than a panic, so a hostile peer can at most end its
/// own connection.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The connection or stream ended, timed out, or was closed by either side; the
    /// string carries the underlying reason for logs.
    #[error("connection closed: {0}")]
    Closed(String),
    /// The peer closed the connection on purpose (see [`Closer::leave`](super::Closer::leave)), as opposed to
    /// being lost to a timeout or the network.
    #[error("peer disconnected")]
    PeerLeft,
    /// The peer closed the connection because it forgot this device (see [`Closer::forget`](super::Closer::forget)).
    /// Only meaningful on a link whose peer is pinned: anyone can send the reason.
    #[error("peer forgot this device")]
    Forgotten,
    /// The peer closed the connection with a reason of the protocol ([`CloseReason`]). The
    /// peer's own words are never kept: an unknown reason is [`CloseReason::Other`].
    #[error("closed by the peer: {0}")]
    Refused(CloseReason),
    /// A frame could not be encoded or decoded, or exceeded its size limit.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The TLS configuration could not be built, or the peer's certificate was missing
    /// or unusable.
    #[error("tls setup: {0}")]
    Tls(String),
    /// A local I/O failure: binding the socket, starting a dial, reading the system RNG
    /// or touching the on-disk store.
    #[error("io: {0}")]
    Io(String),
    /// Control or motion on a link whose peer is not pinned.
    #[error("peer is not trusted")]
    Untrusted,
    /// One transfer stream failed (the peer reset it, finished it early, or never sent its
    /// id) while the connection itself is fine; unlike [`NetError::Closed`] the link keeps
    /// working and the caller should only abandon that transfer.
    #[error("transfer stream failed: {0}")]
    Transfer(String),
}

/// Wraps any displayable error as [`NetError::Tls`], for use with `map_err`.
pub(super) fn tls(e: impl std::fmt::Display) -> NetError {
    NetError::Tls(e.to_string())
}

/// Wraps any displayable error as [`NetError::Io`], for use with `map_err`.
pub(crate) fn io(e: impl std::fmt::Display) -> NetError {
    NetError::Io(e.to_string())
}

/// Wraps any displayable error as [`NetError::Closed`], for use with `map_err`. The text can
/// carry a reason the peer chose, so it is made safe to show like a peer's device name.
pub(super) fn closed(e: impl std::fmt::Display) -> NetError {
    NetError::Closed(crate::text::sanitize_name(&e.to_string()))
}

/// Why this device closed a link on purpose, as the peer learns it. It crosses the wire as a
/// short fixed code (see `CloseReason::code`) and the peer's own text is never shown to the
/// user, so a hostile peer cannot put words on this device's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The dialed device does not know this one and is not open to pairing.
    NotPaired,
    /// The device is busy with another connection.
    Busy,
    /// The device reached is not the one that was expected at that address.
    UnexpectedPeer,
    /// Pairing failed or broke the protocol.
    PairingFailed,
    /// The `Hello` exchange did not finish in time.
    HelloTimedOut,
    /// The device only works with a computer running Overdesk Pro.
    NeedsPro,
    /// Any other reason, including every code this build does not know.
    Other,
}

impl CloseReason {
    /// The one place a reason becomes wire bytes.
    pub(super) fn code(self) -> &'static [u8] {
        match self {
            CloseReason::NotPaired => b"not paired",
            CloseReason::Busy => b"busy",
            CloseReason::UnexpectedPeer => b"unexpected peer",
            CloseReason::PairingFailed => b"pairing failed",
            CloseReason::HelloTimedOut => b"hello timed out",
            CloseReason::NeedsPro => b"needs pro",
            CloseReason::Other => b"other",
        }
    }

    /// The one place wire bytes become a reason; anything unknown is [`CloseReason::Other`].
    fn from_code(code: &[u8]) -> Self {
        [
            CloseReason::NotPaired,
            CloseReason::Busy,
            CloseReason::UnexpectedPeer,
            CloseReason::PairingFailed,
            CloseReason::HelloTimedOut,
            CloseReason::NeedsPro,
        ]
        .into_iter()
        .find(|r| r.code() == code)
        .unwrap_or(CloseReason::Other)
    }
}

impl std::fmt::Display for CloseReason {
    /// A short phrase for logs, not for the user.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(String::from_utf8_lossy(self.code()).as_ref())
    }
}

/// The application close reason sent by [`Closer::leave`].
pub(super) const LEAVE_REASON: &[u8] = b"bye";

/// The application close reason sent by [`Closer::forget`].
pub(super) const FORGET_REASON: &[u8] = b"forgotten";

/// Like [`closed`], but recognises a deliberate close by the peer.
pub(super) fn closed_on(conn: &Connection, e: impl std::fmt::Display) -> NetError {
    match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed(c)) if c.reason == LEAVE_REASON => NetError::PeerLeft,
        Some(ConnectionError::ApplicationClosed(c)) if c.reason == FORGET_REASON => NetError::Forgotten,
        Some(ConnectionError::ApplicationClosed(c)) if !c.reason.is_empty() => {
            NetError::Refused(CloseReason::from_code(&c.reason))
        }
        _ => closed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_close_reason_round_trips_and_a_peers_words_are_other() {
        use CloseReason::*;
        for r in [
            NotPaired,
            Busy,
            UnexpectedPeer,
            PairingFailed,
            HelloTimedOut,
            NeedsPro,
            Other,
        ] {
            assert_eq!(CloseReason::from_code(r.code()), r);
        }
        for words in [&b"Your bank needs you to click here."[..], b"", b"bye", b"BUSY"] {
            assert_eq!(CloseReason::from_code(words), Other);
        }
    }

    #[test]
    fn closed_reasons_are_made_safe_to_show() {
        let NetError::Closed(text) = closed("bad\n\u{202e}reason\x1b[2J") else {
            panic!()
        };
        assert_eq!(text, "badreason[2J");
        let NetError::Closed(text) = closed("x".repeat(500)) else {
            panic!()
        };
        assert_eq!(text.chars().count(), 64);
    }
}
