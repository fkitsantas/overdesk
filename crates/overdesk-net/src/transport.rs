// SPDX-License-Identifier: AGPL-3.0-only
//! One QUIC connection per peer pair: a reliable control stream plus motion datagrams,
//! over mutual TLS 1.3 with self-signed device certificates.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use overdesk_proto::{Control, FrameError, MAX_FRAME_LEN, Motion, Pairing, decode_motion, encode_frame, encode_motion};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use serde::{Serialize, de::DeserializeOwned};

use crate::pairing::{Fingerprint, fingerprint};

/// ALPN protocol identifier negotiated in the TLS handshake. Both sides offer only this
/// value, so a connection from any other QUIC application fails during the handshake
/// instead of reaching the Overdesk framing layer.
pub const ALPN: &[u8] = b"overdesk/1";
/// TLS server name; certificates are pinned by fingerprint, never matched by name.
const SERVER_NAME: &str = "overdesk";
// A vanished peer (crash, sleep, cable) is noticed within ~3 s, so its held keys
// are released and swallowed input comes back quickly.
/// Interval between QUIC keep-alive packets, so an idle but healthy link never reaches
/// [`IDLE_TIMEOUT`].
const KEEP_ALIVE: Duration = Duration::from_secs(1);
/// Time without any packet from the peer after which the connection is declared lost.
const IDLE_TIMEOUT: Duration = Duration::from_secs(3);
/// First byte on the control stream. QUIC only announces a stream to the peer once data
/// is written, so the dialer sends this immediately; it doubles as a stream-format version.
const STREAM_PREAMBLE: u8 = 1;
/// A peer that connects but never opens its control stream is dropped after this.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest pairing message is ~40 bytes; an unpaired peer gets no more than this.
const MAX_PAIRING_FRAME: usize = 128;
/// Upper bound on handshakes in flight on one endpoint, so a burst of incoming
/// connections cannot exhaust memory before any of them is authenticated.
const MAX_INCOMING: usize = 64;

/// Errors raised by the transport layer. Every failure that can be caused by the peer
/// maps to a variant here rather than a panic, so a hostile peer can at most end its
/// own connection.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The connection or stream ended, timed out, or was closed by either side; the
    /// string carries the underlying reason for logs.
    #[error("connection closed: {0}")]
    Closed(String),
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
}

/// Wraps any displayable error as [`NetError::Tls`], for use with `map_err`.
fn tls(e: impl std::fmt::Display) -> NetError {
    NetError::Tls(e.to_string())
}

/// Wraps any displayable error as [`NetError::Io`], for use with `map_err`.
fn io(e: impl std::fmt::Display) -> NetError {
    NetError::Io(e.to_string())
}

/// Wraps any displayable error as [`NetError::Closed`], for use with `map_err`.
fn closed(e: impl std::fmt::Display) -> NetError {
    NetError::Closed(e.to_string())
}

/// This device's long-lived certificate and private key.
///
/// The certificate is self-signed; peers trust it only through the fingerprint pinned
/// at pairing time. The private key never leaves this struct except via the store.
pub struct Identity {
    /// Self-signed X.509 certificate in DER form, presented to every peer.
    cert: CertificateDer<'static>,
    /// PKCS#8 private key matching `cert`; it signs the TLS handshake, which proves to
    /// the peer that this device owns the certificate.
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// Generates a fresh self-signed certificate and key pair for this device.
    ///
    /// shortcut: lives in memory only; persist to the OS keychain before shipping.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Tls`] if key or certificate generation fails.
    pub fn generate() -> Result<Self, NetError> {
        let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()]).map_err(tls)?;
        Ok(Self {
            cert: ck.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        })
    }

    /// SHA-256 fingerprint of this device's certificate: the value peers pin.
    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.cert)
    }

    /// `(certificate DER, PKCS#8 private key DER)` for persisting.
    pub(crate) fn to_der(&self) -> (Vec<u8>, Vec<u8>) {
        (self.cert.to_vec(), self.key.secret_pkcs8_der().to_vec())
    }

    /// Rebuilds an identity from bytes previously produced by [`Identity::to_der`].
    /// The pair is not validated here; a corrupt key is reported as [`NetError::Tls`]
    /// when [`Node::bind`] builds the TLS configuration.
    pub(crate) fn from_der(cert: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            cert: CertificateDer::from(cert),
            key: PrivatePkcs8KeyDer::from(key),
        }
    }

    /// A fresh owned copy of the private key, in the form rustls configuration takes.
    fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

/// Skips certificate-chain validation only: Overdesk trusts by pinned fingerprint,
/// checked after the handshake. Handshake signatures are always verified, so a peer
/// must hold the private key of the certificate it presents.
#[derive(Debug)]
struct PinLater(Arc<CryptoProvider>);

impl PinLater {
    /// Signature schemes the crypto provider can verify, advertised to the peer.
    fn schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for PinLater {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for PinLater {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

/// QUIC transport settings shared by the server and client sides: liveness timers,
/// tight flow-control windows and small datagram buffers, sized for one control stream
/// and a trickle of motion datagrams.
fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("3 s fits a QUIC varint")));
    // Fails only if the timeout exceeded the QUIC varint range, which 3 s never does.
    // One control stream per connection and windows just above one frame: quinn's
    // defaults (100+100 streams, unbounded connection window) let any peer that merely
    // completes TLS make us buffer hundreds of megabytes.
    let frame = MAX_FRAME_LEN as u32 + 4;
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    t.stream_receive_window((2 * frame).into());
    t.receive_window((2 * frame).into());
    t.send_window(4 * frame as u64);
    // Motion datagrams are ~15 bytes; a small buffer drops stale ones under load.
    t.datagram_receive_buffer_size(Some(16 * 1024));
    t.datagram_send_buffer_size(16 * 1024);
    Arc::new(t)
}

/// A bound QUIC endpoint that can both dial and accept.
///
/// Every connection, in either direction, uses mutual TLS 1.3 with this device's
/// [`Identity`]; the peer's certificate is accepted at the TLS layer and only trusted
/// later, by fingerprint, through [`Link::trust`].
pub struct Node {
    /// The underlying quinn endpoint, configured as both server and client.
    endpoint: Endpoint,
    /// This device's certificate fingerprint, copied into every [`Link`].
    local: Fingerprint,
}

impl Node {
    /// Binds a UDP socket at `addr` and configures it to accept and dial with
    /// `identity`. Only TLS 1.3 is offered, client certificates are mandatory, and only
    /// the [`ALPN`] protocol is negotiated.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Tls`] if the TLS configuration cannot be built (for example
    /// a key that does not match the certificate) and [`NetError::Io`] if the socket
    /// cannot be bound.
    pub fn bind(identity: &Identity, addr: SocketAddr) -> Result<Self, NetError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinLater(provider.clone()));

        let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(tls)?
            .with_client_cert_verifier(verifier.clone())
            .with_single_cert(vec![identity.cert.clone()], identity.key())
            .map_err(tls)?;
        server.alpn_protocols = vec![ALPN.to_vec()];
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server).map_err(tls)?));
        server.transport_config(transport());
        // A desk has a handful of devices; bound handshakes in flight.
        server.max_incoming(MAX_INCOMING);

        let mut client = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(tls)?
            // Chain validation is replaced by fingerprint pinning; see `PinLater`.
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(vec![identity.cert.clone()], identity.key())
            .map_err(tls)?;
        client.alpn_protocols = vec![ALPN.to_vec()];
        let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client).map_err(tls)?));
        client.transport_config(transport());

        let mut endpoint = Endpoint::server(server, addr).map_err(io)?;
        endpoint.set_default_client_config(client);
        Ok(Self {
            endpoint,
            local: identity.fingerprint(),
        })
    }

    /// The socket address the endpoint is bound to, including the port the OS chose
    /// when binding to port 0.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Io`] if the socket address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        self.endpoint.local_addr().map_err(io)
    }

    /// Dials `addr` and opens the control stream.
    ///
    /// The returned [`Link`] is authenticated (the peer proved it holds its
    /// certificate's key) but not yet trusted.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Io`] if the dial cannot start, [`NetError::Closed`] if the
    /// handshake or stream setup fails, and [`NetError::Tls`] if the peer presented no
    /// usable certificate.
    pub async fn connect(&self, addr: SocketAddr) -> Result<Link, NetError> {
        let conn = self
            .endpoint
            .connect(addr, SERVER_NAME)
            .map_err(io)?
            .await
            .map_err(closed)?;
        let (mut send, recv) = conn.open_bi().await.map_err(closed)?;
        // Without this write the listener would never see the stream and would time out.
        send.write_all(&[STREAM_PREAMBLE]).await.map_err(closed)?;
        Link::new(conn, send, recv, self.local, Role::Dialer)
    }

    /// Waits for the next incoming connection and returns its setup as a separate
    /// future, so a stalled peer cannot hold up others: spawn one per connection.
    /// Setup (handshake, control stream, preamble) is bounded by [`SETUP_TIMEOUT`].
    /// `None` once the endpoint is closed.
    pub async fn accept(&self) -> Option<impl Future<Output = Result<Link, NetError>> + use<>> {
        let incoming = loop {
            let incoming = self.endpoint.accept().await?;
            // Make unvalidated sources prove their address first (QUIC Retry), so spoofed
            // floods cost us a stateless packet instead of a handshake.
            if !incoming.remote_address_validated() && incoming.may_retry() {
                let _ = incoming.retry();
                continue;
            }
            break incoming;
        };
        let local = self.local;
        Some(async move {
            let setup = async {
                let conn = incoming.await.map_err(closed)?;
                let (send, mut recv) = conn.accept_bi().await.map_err(closed)?;
                let mut preamble = [0u8];
                recv.read_exact(&mut preamble).await.map_err(closed)?;
                if preamble[0] != STREAM_PREAMBLE {
                    conn.close(1u32.into(), b"unknown stream format");
                    return Err(closed("unknown stream format"));
                }
                Link::new(conn, send, recv, local, Role::Listener)
            };
            // Dropping the timed-out setup drops the connection, which closes it.
            tokio::time::timeout(SETUP_TIMEOUT, setup)
                .await
                .map_err(|_| closed("setup timed out"))?
        })
    }
}

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
/// pairing messages can be exchanged: control and motion return `Untrusted`.
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
    /// Set only by a successful [`Link::trust`]; gates every control and motion call.
    trusted: bool,
}

impl Link {
    /// Wraps an established connection and its control stream, reading the peer's
    /// fingerprint from the leaf certificate TLS verified the handshake signature with.
    /// The link starts untrusted.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Tls`] if the peer presented no certificate.
    fn new(
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        local: Fingerprint,
        role: Role,
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

    /// Unlocks control and motion if `pinned` accepts the peer's fingerprint.
    ///
    /// A rejection also revokes any earlier trust on this link.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] if `pinned` rejects the fingerprint.
    pub fn trust(&mut self, pinned: impl Fn(&Fingerprint) -> bool) -> Result<(), NetError> {
        self.trusted = pinned(&self.peer);
        if self.trusted { Ok(()) } else { Err(NetError::Untrusted) }
    }

    /// Guard called at the top of every control, motion and split operation.
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
        self.send_frame(msg).await
    }

    /// Receives one pairing message. Allowed before trust, but frames are capped at
    /// `MAX_PAIRING_FRAME` (128) bytes so an unpaired peer cannot make us buffer more.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Frame`] for an oversized or undecodable frame (an oversized
    /// one also closes the connection) and [`NetError::Closed`] if the stream ends.
    pub async fn recv_pairing(&mut self) -> Result<Pairing, NetError> {
        self.recv_frame(MAX_PAIRING_FRAME).await
    }

    /// Sends one control message on the reliable, ordered stream.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] before [`Link::trust`] succeeds,
    /// [`NetError::Frame`] if the message is too large to encode, and
    /// [`NetError::Closed`] if the stream is gone.
    pub async fn send_control(&mut self, msg: &Control) -> Result<(), NetError> {
        self.require_trust()?;
        self.send_frame(msg).await
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
        self.recv_frame(MAX_FRAME_LEN).await
    }

    /// Writes one length-prefixed frame on the control stream, without a trust check.
    ///
    /// # Errors
    ///
    /// See [`write_frame`].
    async fn send_frame<T: Serialize>(&mut self, msg: &T) -> Result<(), NetError> {
        write_frame(&mut self.send, msg).await
    }

    /// Reads one frame of at most `max_len` bytes from the control stream, without a
    /// trust check.
    ///
    /// # Errors
    ///
    /// See [`read_frame`].
    async fn recv_frame<T: DeserializeOwned>(&mut self, max_len: usize) -> Result<T, NetError> {
        read_frame(&mut self.recv, &self.conn, max_len).await
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

    /// Sends one motion sample as an unreliable QUIC datagram: lost or late samples
    /// are never retransmitted, because a newer one supersedes them.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] before [`Link::trust`] succeeds and
    /// [`NetError::Closed`] if the datagram cannot be sent.
    pub fn send_motion(&self, m: &Motion) -> Result<(), NetError> {
        self.require_trust()?;
        self.conn.send_datagram(encode_motion(m).into()).map_err(closed)
    }

    /// Waits for the next motion datagram.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Untrusted`] before [`Link::trust`] succeeds,
    /// [`NetError::Closed`] once the connection is gone, and [`NetError::Frame`] for a
    /// malformed datagram.
    pub async fn recv_motion(&self) -> Result<Motion, NetError> {
        self.require_trust()?;
        let d = self.conn.read_datagram().await.map_err(closed)?;
        Ok(decode_motion(&d)?)
    }

    /// Closes the connection immediately, sending `reason` to the peer. Data still
    /// queued on the control stream may be discarded.
    pub fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }

    /// Resolves when the connection is gone (closed, timed out, or lost).
    pub async fn closed(&self) -> String {
        self.conn.closed().await.to_string()
    }
}

/// Encodes `msg` as one length-prefixed frame and writes it in full to `send`.
///
/// # Errors
///
/// Returns [`NetError::Frame`] if the message cannot be encoded or exceeds
/// [`MAX_FRAME_LEN`], and [`NetError::Closed`] if the stream is gone.
async fn write_frame<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<(), NetError> {
    let frame = encode_frame(msg)?;
    send.write_all(&frame).await.map_err(closed)
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
async fn read_frame<T: DeserializeOwned>(
    recv: &mut RecvStream,
    conn: &Connection,
    max_len: usize,
) -> Result<T, NetError> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(closed)?;
    let n = u32::from_be_bytes(len) as usize;
    if n > max_len {
        conn.close(0u32.into(), b"frame too large");
        return Err(FrameError::TooLarge(n).into());
    }
    let mut frame = len.to_vec();
    while frame.len() < 4 + n {
        let want = 4 + n - frame.len();
        let chunk = recv.read_chunk(want, true).await.map_err(closed)?;
        let chunk = chunk.ok_or_else(|| closed("stream ended mid-frame"))?;
        frame.extend_from_slice(&chunk.bytes);
    }
    match overdesk_proto::decode_frame(&frame)? {
        Some((msg, _)) => Ok(msg),
        None => unreachable!("frame is complete by construction"),
    }
}

/// Writer half of a trusted [`Link`].
///
/// Trust was checked when the link was split, so its methods do not check again.
pub struct LinkTx {
    /// The shared QUIC connection, used for datagrams and closing.
    conn: Connection,
    /// Sending half of the control stream; owned by this half alone.
    send: SendStream,
}

impl LinkTx {
    /// Sends one control message on the reliable, ordered stream.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Frame`] if the message is too large to encode and
    /// [`NetError::Closed`] if the stream is gone.
    pub async fn send_control(&mut self, msg: &Control) -> Result<(), NetError> {
        write_frame(&mut self.send, msg).await
    }

    /// Sends one motion sample as an unreliable QUIC datagram.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Closed`] if the datagram cannot be sent.
    pub fn send_motion(&self, m: &Motion) -> Result<(), NetError> {
        self.conn.send_datagram(encode_motion(m).into()).map_err(closed)
    }

    /// Closes the whole connection, which also ends the reader half.
    pub fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }
}

/// Reader half of a trusted [`Link`].
///
/// Trust was checked when the link was split, so its methods do not check again.
pub struct LinkRx {
    /// The shared QUIC connection, used for datagrams and close notification.
    conn: Connection,
    /// Receiving half of the control stream; owned by this half alone.
    recv: RecvStream,
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

    /// Waits for the next motion datagram.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Closed`] once the connection is gone and
    /// [`NetError::Frame`] for a malformed datagram.
    pub async fn recv_motion(&self) -> Result<Motion, NetError> {
        let d = self.conn.read_datagram().await.map_err(closed)?;
        Ok(decode_motion(&d)?)
    }

    /// A separate motion reader, so datagrams and the control stream can be read by
    /// different tasks (the control read must never be cancelled mid-frame).
    pub fn motion(&self) -> MotionRx {
        MotionRx {
            conn: self.conn.clone(),
        }
    }

    /// Resolves when the connection is gone (closed, timed out, or lost).
    pub async fn closed(&self) -> String {
        self.conn.closed().await.to_string()
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
        let d = self.conn.read_datagram().await.map_err(closed)?;
        Ok(decode_motion(&d)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> (Identity, Node) {
        let id = Identity::generate().unwrap();
        let node = Node::bind(&id, "127.0.0.1:0".parse().unwrap()).unwrap();
        (id, node)
    }

    /// Accepts the next connection and runs its setup in the background, as a real caller does.
    async fn spawn_next_setup(server: &Node) -> tokio::task::JoinHandle<Result<Link, NetError>> {
        tokio::spawn(server.accept().await.unwrap())
    }

    /// Completes TLS but never opens the control stream.
    async fn stalled_peer(addr: SocketAddr) -> (Node, Connection) {
        let (_, n) = node();
        let conn = n.endpoint.connect(addr, SERVER_NAME).unwrap().await.unwrap();
        (n, conn)
    }

    #[tokio::test]
    async fn stalled_peer_does_not_block_accepting_others() {
        let (_, server) = node();
        let addr = server.local_addr().unwrap();
        // Spawned like a real caller would: the handshake runs, the stream never comes.
        let (stalled_setup, (_slow_node, _slow_conn)) = tokio::join!(spawn_next_setup(&server), stalled_peer(addr));
        let (fast_id, fast) = node();
        let (dialed, link) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(fast.connect(addr), async { server.accept().await.unwrap().await })
        })
        .await
        .expect("second peer was blocked behind the stalled one");
        dialed.unwrap();
        assert_eq!(link.unwrap().peer_fingerprint(), fast_id.fingerprint());
        assert!(!stalled_setup.is_finished());
        stalled_setup.abort();
    }

    #[tokio::test]
    async fn stalled_setup_times_out() {
        let (_, server) = node();
        let addr = server.local_addr().unwrap();
        let started = std::time::Instant::now();
        let (setup, (_n, _c)) = tokio::join!(spawn_next_setup(&server), stalled_peer(addr));
        let result = setup.await.unwrap();
        assert!(matches!(result, Err(NetError::Closed(_))));
        assert!(started.elapsed() < SETUP_TIMEOUT + Duration::from_secs(2));
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
