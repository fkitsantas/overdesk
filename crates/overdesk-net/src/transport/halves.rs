// SPDX-License-Identifier: AGPL-3.0-only
//! What a trusted link splits into: the writer, the reader, and the handles for motion datagrams
//! and transfer streams that other tasks can hold.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use quinn::{Connection, RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite};

use overdesk_proto::{Control, MAX_FRAME_LEN, Motion, decode_motion, encode_motion};

use super::error::{CloseReason, FORGET_REASON, LEAVE_REASON, NetError, closed, closed_on};
use super::link::{read_frame, write_frame, write_frame_timeout};
use super::node::SETUP_TIMEOUT;

/// How long the sender of a transfer stream has to write its 16-byte id.
const TRANSFER_ID_TIMEOUT: Duration = SETUP_TIMEOUT;

/// A handle that can close a running link from outside its tasks.
#[derive(Clone)]
pub struct Closer(Connection);

impl Closer {
    /// Closes the connection with the "bye" reason, so the peer sees a deliberate
    /// disconnect ([`NetError::PeerLeft`]) rather than a lost link.
    pub fn leave(&self) {
        self.0.close(0u32.into(), LEAVE_REASON);
    }

    /// Closes the connection with `reason`, which the peer sees as [`NetError::Refused`]. For a
    /// link that must not be used again, such as one whose control write was cancelled mid-frame.
    pub fn close(&self, reason: CloseReason) {
        self.0.close(0u32.into(), reason.code());
    }

    /// Closes the connection with the "forgotten" reason: this device has unpaired the peer,
    /// which sees [`NetError::Forgotten`] and may unpair this device in turn.
    pub fn forget(&self) {
        self.0.close(0u32.into(), FORGET_REASON);
    }
}

/// Sends one motion sample as an unreliable QUIC datagram: lost or late samples are never
/// retransmitted, because a newer one supersedes them.
///
/// # Errors
///
/// Returns [`NetError::Closed`] if the datagram cannot be sent.
fn send_dgram(conn: &Connection, m: &Motion) -> Result<(), NetError> {
    conn.send_datagram(encode_motion(m).into()).map_err(closed)
}

/// Waits for the next motion datagram on `conn`.
///
/// # Errors
///
/// Returns [`NetError::Closed`] once the connection is gone and [`NetError::Frame`] for a
/// malformed datagram.
async fn recv_dgram(conn: &Connection) -> Result<Motion, NetError> {
    let d = conn.read_datagram().await.map_err(closed)?;
    Ok(decode_motion(&d)?)
}

/// Writer of one transfer stream. Shutting it down finishes the stream, which the
/// receiver sees as a complete transfer. Dropping it any other way (an error, a cancelled
/// task, a panic) resets the stream instead, so an aborted sender can never pass for a
/// complete one: the receiver's reads fail.
pub struct TransferWriter {
    /// The stream; `None` once shut down cleanly.
    send: Option<SendStream>,
}

impl AsyncWrite for TransferWriter {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut().send.as_mut() {
            Some(s) => AsyncWrite::poll_write(Pin::new(s), cx, buf),
            None => Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut().send.as_mut() {
            Some(s) => AsyncWrite::poll_flush(Pin::new(s), cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let Some(s) = this.send.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        let r = AsyncWrite::poll_shutdown(Pin::new(s), cx);
        if matches!(r, Poll::Ready(Ok(()))) {
            // Finished: disarm the reset-on-drop.
            if let Some(mut done) = this.send.take() {
                let _ = done.finish();
            }
        }
        r
    }
}

impl Drop for TransferWriter {
    fn drop(&mut self) {
        if let Some(s) = self.send.as_mut() {
            let _ = s.reset(TRANSFER_ABORTED.into());
        }
    }
}

/// Reset code of a transfer stream dropped before it was shut down.
const TRANSFER_ABORTED: u32 = 1;

/// Opens a uni stream on `conn` and writes the 16-byte transfer `id`; the caller writes
/// the content and shuts the writer down to end it.
///
/// # Errors
///
/// Returns [`NetError::Closed`] if the connection is gone. Against a peer bound without
/// transfers it never completes (no stream credit): bound it with a timeout.
async fn open_transfer(conn: &Connection, id: [u8; 16]) -> Result<TransferWriter, NetError> {
    let mut send = conn.open_uni().await.map_err(closed)?;
    send.write_all(&id).await.map_err(closed)?;
    Ok(TransferWriter { send: Some(send) })
}

/// Waits for the peer's next transfer stream and reads its 16-byte id, which must arrive
/// within [`TRANSFER_ID_TIMEOUT`]. Not cancel-safe once a stream has arrived: dropping it
/// mid-way discards that transfer.
///
/// # Errors
///
/// Returns [`NetError::Closed`] (or [`NetError::PeerLeft`]) if the connection is gone, and
/// [`NetError::Transfer`] if only that stream failed: reset, finished before its id, or
/// silent for too long. The link stays usable after a `Transfer` error.
async fn accept_transfer(conn: &Connection) -> Result<([u8; 16], impl AsyncRead + Unpin + use<>), NetError> {
    let mut recv = conn.accept_uni().await.map_err(closed)?;
    let mut id = [0u8; 16];
    match tokio::time::timeout(TRANSFER_ID_TIMEOUT, recv.read_exact(&mut id)).await {
        Ok(Ok(())) => Ok((id, recv)),
        Ok(Err(quinn::ReadExactError::ReadError(quinn::ReadError::Reset(code)))) => {
            Err(NetError::Transfer(format!("reset by peer before its id (code {code})")))
        }
        Ok(Err(quinn::ReadExactError::FinishedEarly(n))) => {
            Err(NetError::Transfer(format!("finished after {n} of 16 id bytes")))
        }
        Ok(Err(e)) => Err(closed_on(conn, e)),
        Err(_) => {
            let _ = recv.stop(TRANSFER_ABORTED.into());
            Err(NetError::Transfer("no id within the setup timeout".into()))
        }
    }
}

/// Opens transfer streams on a trusted link from any task; see [`LinkTx::transfer_opener`].
/// Cheap to clone.
#[derive(Clone)]
pub struct TransferOpener(Connection);

impl TransferOpener {
    /// Like [`LinkTx::open_transfer`]. Wrap it in a timeout: a peer that never grants a
    /// stream (one bound without transfers) leaves it pending.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Closed`] once the connection is gone.
    pub async fn open_transfer(&self, id: [u8; 16]) -> Result<TransferWriter, NetError> {
        open_transfer(&self.0, id).await
    }
}

/// Writer half of a trusted [`Link`](super::Link).
///
/// Trust was checked when the link was split, so its methods do not check again.
pub struct LinkTx {
    /// The shared QUIC connection, used for datagrams and closing.
    pub(super) conn: Connection,
    /// Sending half of the control stream; owned by this half alone.
    pub(super) send: SendStream,
}

impl LinkTx {
    /// Sends one control message on the reliable, ordered stream. Not cancel-safe
    /// mid-frame (see [`Link::send_control`](super::Link::send_control)) and blocks for good on a peer that stops
    /// reading; see [`LinkTx::send_control_timeout`].
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Frame`] if the message is too large to encode and
    /// [`NetError::Closed`] if the stream is gone.
    pub async fn send_control(&mut self, msg: &Control) -> Result<(), NetError> {
        write_frame(&mut self.send, msg).await
    }

    /// Like [`LinkTx::send_control`], but closes the connection and returns
    /// [`NetError::Closed`] if the frame is not fully accepted within `limit`.
    ///
    /// # Errors
    ///
    /// As for [`LinkTx::send_control`], plus the timeout.
    pub async fn send_control_timeout(&mut self, msg: &Control, limit: Duration) -> Result<(), NetError> {
        write_frame_timeout(&mut self.send, &self.conn, msg, limit).await
    }

    /// Sends one motion sample as an unreliable QUIC datagram.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Closed`] if the datagram cannot be sent.
    pub fn send_motion(&self, m: &Motion) -> Result<(), NetError> {
        send_dgram(&self.conn, m)
    }

    /// Opens a transfer stream; see [`LinkTx::open_transfer`].
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Closed`] if the link is gone; never completes against a peer
    /// without transfers.
    pub async fn open_transfer(&self, id: [u8; 16]) -> Result<TransferWriter, NetError> {
        open_transfer(&self.conn, id).await
    }

    /// A handle that can close this link from another task.
    pub fn closer(&self) -> Closer {
        Closer(self.conn.clone())
    }

    /// A handle that opens transfer streams from another task, so a peer slow to grant one
    /// never holds up the task sending control messages.
    pub fn transfer_opener(&self) -> TransferOpener {
        TransferOpener(self.conn.clone())
    }

    /// A handle that sends motion datagrams from any task, so a sample never waits for the
    /// task writing control messages (datagrams do not use the control stream).
    pub fn motion_sender(&self) -> MotionSender {
        MotionSender(self.conn.clone())
    }

    /// Closes the whole connection, which also ends the reader half.
    pub fn close(&self, reason: CloseReason) {
        self.conn.close(0u32.into(), reason.code());
    }
}

/// Sends motion datagrams on a trusted link from any task; see [`LinkTx::motion_sender`].
/// Cheap to clone.
#[derive(Clone)]
pub struct MotionSender(Connection);

impl MotionSender {
    /// Like [`LinkTx::send_motion`].
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Closed`] if the datagram cannot be sent.
    pub fn send(&self, m: &Motion) -> Result<(), NetError> {
        send_dgram(&self.0, m)
    }
}

/// Reader half of a trusted [`Link`](super::Link).
///
/// Trust was checked when the link was split, so its methods do not check again.
pub struct LinkRx {
    /// The shared QUIC connection, used for datagrams and close notification.
    pub(super) conn: Connection,
    /// Receiving half of the control stream; owned by this half alone.
    pub(super) recv: RecvStream,
}

impl LinkRx {
    /// Receives one control message, up to [`MAX_FRAME_LEN`] bytes. Not cancel-safe:
    /// abandoning it mid-frame desynchronises the stream.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Frame`] for an oversized or undecodable frame and
    /// [`NetError::Closed`] if the stream ends.
    pub async fn recv_control(&mut self) -> Result<Control, NetError> {
        read_frame(&mut self.recv, &self.conn, MAX_FRAME_LEN).await
    }

    /// A separate transfer acceptor, so incoming transfers can be awaited by a task of
    /// their own while another reads the control stream.
    pub fn transfers(&self) -> TransferRx {
        TransferRx {
            conn: self.conn.clone(),
        }
    }

    /// A separate motion reader, so datagrams and the control stream can be read by
    /// different tasks (the control read must never be cancelled mid-frame).
    pub fn motion(&self) -> MotionRx {
        MotionRx {
            conn: self.conn.clone(),
        }
    }
}

/// Transfer-stream acceptor of a trusted link.
pub struct TransferRx {
    /// A clone of the trusted connection; only its uni streams are accepted.
    conn: Connection,
}

impl TransferRx {
    /// Waits for the peer's next transfer stream; see [`LinkRx::transfers`].
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Closed`] once the connection is gone.
    pub async fn accept(&self) -> Result<([u8; 16], impl AsyncRead + Unpin + use<>), NetError> {
        accept_transfer(&self.conn).await
    }
}

/// Motion-lane reader of a trusted link.
pub struct MotionRx {
    /// A clone of the trusted connection; only its datagrams are read.
    conn: Connection,
}

impl MotionRx {
    /// Waits for the next motion datagram. Cancel-safe, so it can sit in a `select!`.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Closed`] once the connection is gone and
    /// [`NetError::Frame`] for a malformed datagram.
    pub async fn recv(&self) -> Result<Motion, NetError> {
        recv_dgram(&self.conn).await
    }
}
