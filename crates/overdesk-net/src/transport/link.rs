// SPDX-License-Identifier: AGPL-3.0-only
//! An authenticated connection to one peer, and the length-prefixed frames on its control stream.

use std::net::SocketAddr;
use std::time::Duration;

use quinn::{Connection, RecvStream, SendStream};
use rustls::pki_types::CertificateDer;
use serde::{Serialize, de::DeserializeOwned};

use overdesk_proto::{Control, FrameError, LEN_PREFIX, MAX_FRAME_LEN, Pairing, encode_frame};

use super::error::{CloseReason, NetError, closed, closed_on, tls};
use super::halves::{LinkRx, LinkTx};
use super::node::PRE_TRUST_WINDOW;
use crate::pairing::{Fingerprint, fingerprint};

/// Largest pairing message is ~40 bytes; an unpaired peer gets no more than this.
const MAX_PAIRING_FRAME: usize = 128;

/// Which side opened the connection. Fixed at creation, so callers cannot get the
/// pairing order wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// This side opened the connection and plays the committing side of pairing.
    Dialer,
    /// This side accepted the connection and plays the nonce-first side of pairing.
    Listener,
}

/// An authenticated connection to one peer. Until [`Link::trust`] succeeds, only
/// pairing messages can be exchanged: control returns `Untrusted` and [`Link::split`], the way to
/// motion and transfers, is refused.
pub struct Link {
    /// The QUIC connection, used for datagrams, closing and peer metadata.
    conn: Connection,
    /// Sending half of the single bidirectional control stream.
    send: SendStream,
    /// Receiving half of the single bidirectional control stream.
    recv: RecvStream,
    /// Fingerprint of the certificate the peer authenticated with in this handshake.
    peer: Fingerprint,
    /// This device's own certificate fingerprint.
    local: Fingerprint,
    /// Which side opened the connection; decides the pairing role.
    role: Role,
    /// Set only by a successful [`Link::trust`]; gates every control call and splitting.
    trusted: bool,
    /// The connection receive window a trusted peer gets; an untrusted one has [`PRE_TRUST_WINDOW`].
    trusted_window: u32,
}

impl Link {
    /// Wraps an established connection and its control stream, reading the peer's
    /// fingerprint from the leaf certificate TLS verified the handshake signature with.
    /// The link starts untrusted.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Tls`] if the peer presented no certificate.
    pub(super) fn new(
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        local: Fingerprint,
        role: Role,
        trusted_window: u32,
    ) -> Result<Self, NetError> {
        let certs = conn
            .peer_identity()
            .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
            .ok_or_else(|| tls("peer presented no certificate"))?;
        let leaf = certs.first().ok_or_else(|| tls("empty certificate chain"))?;
        Ok(Self {
            peer: fingerprint(leaf),
            local,
            role,
            trusted: false,
            trusted_window,
            conn,
            send,
            recv,
        })
    }

    /// The fingerprint of the certificate the peer proved it holds the key for.
    pub fn peer_fingerprint(&self) -> Fingerprint {
        self.peer
    }

    /// This device's own fingerprint.
    pub fn local_fingerprint(&self) -> Fingerprint {
        self.local
    }

    /// Which side opened this connection.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The peer's network address, for display before its name is known.
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Unlocks control, splitting, motion and transfers if `pinned` accepts the peer's fingerprint.
    ///
    /// A rejection also revokes any earlier trust on this link.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] if `pinned` rejects the fingerprint.
    pub fn trust(&mut self, pinned: impl Fn(&Fingerprint) -> bool) -> Result<(), NetError> {
        self.trusted = pinned(&self.peer);
        // Memory is committed to a peer only once it is pinned, and taken back if it is not.
        let window = if self.trusted {
            self.trusted_window
        } else {
            PRE_TRUST_WINDOW
        };
        self.conn.set_receive_window(window.into());
        if self.trusted { Ok(()) } else { Err(NetError::Untrusted) }
    }

    /// Guard called at the top of every control and split operation.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] unless [`Link::trust`] has succeeded.
    fn require_trust(&self) -> Result<(), NetError> {
        if self.trusted { Ok(()) } else { Err(NetError::Untrusted) }
    }

    /// Sends one pairing message. Allowed before trust, since pairing is how trust is
    /// established.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Frame`] if encoding fails and [`NetError::Closed`] if the
    /// stream is gone.
    pub async fn send_pairing(&mut self, msg: &Pairing) -> Result<(), NetError> {
        write_frame(&mut self.send, msg).await
    }

    /// Receives one pairing message. Allowed before trust, but frames are capped at
    /// `MAX_PAIRING_FRAME` (128) bytes so an unpaired peer cannot make us buffer more.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Frame`] for an oversized or undecodable frame (an oversized
    /// one also closes the connection) and [`NetError::Closed`] if the stream ends.
    pub async fn recv_pairing(&mut self) -> Result<Pairing, NetError> {
        read_frame(&mut self.recv, &self.conn, MAX_PAIRING_FRAME).await
    }

    /// Sends one control message on the reliable, ordered stream.
    ///
    /// Not cancel-safe: dropping the future mid-frame leaves a partial frame on the stream
    /// and desynchronises it, so close the link after abandoning it. A peer that stops
    /// reading stalls it for good; use [`Link::send_control_timeout`] where that matters.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] before [`Link::trust`] succeeds,
    /// [`NetError::Frame`] if the message is too large to encode, and
    /// [`NetError::Closed`] if the stream is gone.
    pub async fn send_control(&mut self, msg: &Control) -> Result<(), NetError> {
        self.require_trust()?;
        write_frame(&mut self.send, msg).await
    }

    /// Like [`Link::send_control`], but if the frame is not fully accepted within `limit`
    /// (the peer is stalled) the connection is closed and [`NetError::Closed`] returned.
    ///
    /// # Errors
    ///
    /// As for [`Link::send_control`], plus the timeout.
    pub async fn send_control_timeout(&mut self, msg: &Control, limit: Duration) -> Result<(), NetError> {
        self.require_trust()?;
        write_frame_timeout(&mut self.send, &self.conn, msg, limit).await
    }

    /// Receives one control message, up to [`MAX_FRAME_LEN`] bytes.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] before [`Link::trust`] succeeds,
    /// [`NetError::Frame`] for an oversized or undecodable frame, and
    /// [`NetError::Closed`] if the stream ends.
    pub async fn recv_control(&mut self) -> Result<Control, NetError> {
        self.require_trust()?;
        read_frame(&mut self.recv, &self.conn, MAX_FRAME_LEN).await
    }

    /// Splits a trusted link into independent writer and reader halves, so one task
    /// can read while another writes. Untrusted links never split.
    ///
    /// The halves carry no trust flag of their own: trust is checked once here, which
    /// is why only a trusted link may be split.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] if the link is not trusted.
    pub fn split(self) -> Result<(LinkTx, LinkRx), NetError> {
        self.require_trust()?;
        let Link { conn, send, recv, .. } = self;
        Ok((
            LinkTx {
                conn: conn.clone(),
                send,
            },
            LinkRx { conn, recv },
        ))
    }

    /// Closes the connection immediately, sending `reason` to the peer. Data still
    /// queued on the control stream may be discarded.
    pub fn close(&self, reason: CloseReason) {
        self.conn.close(0u32.into(), reason.code());
    }
}

/// Encodes `msg` as one length-prefixed frame and writes it in full to `send`.
///
/// # Errors
///
/// Returns [`NetError::Frame`] if the message cannot be encoded or exceeds
/// [`MAX_FRAME_LEN`], and [`NetError::Closed`] if the stream is gone.
pub(super) async fn write_frame<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<(), NetError> {
    let frame = encode_frame(msg)?;
    send.write_all(&frame).await.map_err(closed)
}

/// [`write_frame`] bounded by `limit`; on expiry closes `conn`, because the frame may be
/// half written and the stream is unusable.
///
/// # Errors
///
/// As for [`write_frame`], and [`NetError::Closed`] on timeout.
pub(super) async fn write_frame_timeout<T: Serialize>(
    send: &mut SendStream,
    conn: &Connection,
    msg: &T,
    limit: Duration,
) -> Result<(), NetError> {
    if let Ok(r) = tokio::time::timeout(limit, write_frame(send, msg)).await {
        r
    } else {
        conn.close(0u32.into(), b"send stalled");
        Err(closed("send timed out"))
    }
}

/// Reads one frame: 4-byte length, limit check, then the body as it arrives, so a
/// peer that announces a large frame and dribbles bytes cannot pin `max_len` of memory.
/// Not cancel-safe: abandoning it mid-frame desynchronises the stream, so give each
/// stream a single reader that always runs to completion.
///
/// # Errors
///
/// Returns [`NetError::Frame`] if the announced length exceeds `max_len` (the
/// connection is closed too) or the body does not decode, and [`NetError::Closed`] if
/// the stream ends before the frame is complete.
pub(super) async fn read_frame<T: DeserializeOwned>(
    recv: &mut RecvStream,
    conn: &Connection,
    max_len: usize,
) -> Result<T, NetError> {
    let mut len = [0u8; LEN_PREFIX];
    recv.read_exact(&mut len).await.map_err(|e| closed_on(conn, e))?;
    let n = u32::from_be_bytes(len) as usize;
    if n > max_len {
        conn.close(0u32.into(), b"frame too large");
        return Err(FrameError::TooLarge(n).into());
    }
    // Small frames (the common case) get one allocation; large ones grow as bytes arrive.
    let mut frame = Vec::with_capacity(if n <= 4096 { LEN_PREFIX + n } else { LEN_PREFIX });
    frame.extend_from_slice(&len);
    while frame.len() < LEN_PREFIX + n {
        let want = LEN_PREFIX + n - frame.len();
        let chunk = recv.read_chunk(want, true).await.map_err(|e| closed_on(conn, e))?;
        let chunk = chunk.ok_or_else(|| closed("stream ended mid-frame"))?;
        frame.extend_from_slice(&chunk.bytes);
    }
    // The frame is complete by construction; `None` would be a bug, reported rather than a panic.
    match overdesk_proto::decode_frame(&frame)? {
        Some((msg, _)) => Ok(msg),
        None => Err(FrameError::Malformed("frame incomplete after its length was read".into()).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::node::node;
    use super::*;

    #[tokio::test]
    async fn an_untrusted_peer_fills_only_a_small_window_and_trust_opens_it() {
        let (_, server) = node();
        let addr = server.local_addr().unwrap();
        let (_, client) = node();
        let (dialer, listener) = tokio::join!(client.connect(addr), async { server.accept().await.unwrap().await });
        let (mut dialer, mut listener) = (dialer.unwrap(), listener.unwrap());
        let chunk = vec![0u8; 64 * 1024];
        let short = Duration::from_millis(300);
        assert!(
            tokio::time::timeout(short, dialer.send.write_all(&chunk))
                .await
                .is_err(),
            "64 KiB must not fit before trust"
        );
        assert!(listener.trust(|_| true).is_ok());
        tokio::time::timeout(Duration::from_secs(2), dialer.send.write_all(&chunk))
            .await
            .expect("room once the peer is trusted")
            .unwrap();
        assert!(listener.trust(|_| false).is_err());
    }

    #[tokio::test]
    async fn peer_cannot_open_extra_streams() {
        let (_, server) = node();
        let addr = server.local_addr().unwrap();
        let (_, client) = node();
        let (link, _accepted) = tokio::join!(client.connect(addr), async { server.accept().await.unwrap().await });
        let conn = link.unwrap().conn;
        let short = Duration::from_millis(300);
        assert!(
            tokio::time::timeout(short, conn.open_bi()).await.is_err(),
            "second bidi stream allowed"
        );
        assert!(
            tokio::time::timeout(short, conn.open_uni()).await.is_err(),
            "uni stream allowed"
        );
    }
}
