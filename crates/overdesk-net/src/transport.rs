// SPDX-License-Identifier: AGPL-3.0-only
//! One QUIC connection per peer pair: a reliable control stream plus motion datagrams,
//! over mutual TLS 1.3 with self-signed device certificates.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use overdesk_proto::{FrameError, MAX_FRAME_LEN, Motion, decode_motion, encode_frame, encode_motion};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use serde::{Serialize, de::DeserializeOwned};

use crate::pairing::{Fingerprint, fingerprint};

pub const ALPN: &[u8] = b"overdesk/1";
/// TLS server name; certificates are pinned by fingerprint, never matched by name.
const SERVER_NAME: &str = "overdesk";
const KEEP_ALIVE: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// First byte on the control stream. QUIC only announces a stream to the peer once data
/// is written, so the dialer sends this immediately; it doubles as a stream-format version.
const STREAM_PREAMBLE: u8 = 1;
/// A peer that connects but never opens its control stream is dropped after this.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("connection closed: {0}")]
    Closed(String),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("tls setup: {0}")]
    Tls(String),
    #[error("io: {0}")]
    Io(String),
}

fn tls(e: impl std::fmt::Display) -> NetError {
    NetError::Tls(e.to_string())
}

fn io(e: impl std::fmt::Display) -> NetError {
    NetError::Io(e.to_string())
}

fn closed(e: impl std::fmt::Display) -> NetError {
    NetError::Closed(e.to_string())
}

/// This device's long-lived certificate and private key.
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// shortcut: lives in memory only; persist to the OS keychain before shipping.
    pub fn generate() -> Result<Self, NetError> {
        let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.into()]).map_err(tls)?;
        Ok(Self {
            cert: ck.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.cert)
    }

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

fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("10 s fits a QUIC varint")));
    Arc::new(t)
}

/// A bound QUIC endpoint that can both dial and accept.
pub struct Node {
    endpoint: Endpoint,
    local: Fingerprint,
}

impl Node {
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

        let mut client = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(tls)?
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

    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        self.endpoint.local_addr().map_err(io)
    }

    /// Dials `addr` and opens the control stream.
    pub async fn connect(&self, addr: SocketAddr) -> Result<Link, NetError> {
        let conn = self
            .endpoint
            .connect(addr, SERVER_NAME)
            .map_err(io)?
            .await
            .map_err(closed)?;
        let (mut send, recv) = conn.open_bi().await.map_err(closed)?;
        send.write_all(&[STREAM_PREAMBLE]).await.map_err(closed)?;
        Link::new(conn, send, recv, self.local)
    }

    /// Waits for the next incoming connection and returns its setup as a separate
    /// future, so a stalled peer cannot hold up others: spawn one per connection.
    /// Setup (handshake, control stream, preamble) is bounded by [`SETUP_TIMEOUT`].
    /// `None` once the endpoint is closed.
    pub async fn accept(&self) -> Option<impl Future<Output = Result<Link, NetError>> + use<>> {
        let incoming = self.endpoint.accept().await?;
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
                Link::new(conn, send, recv, local)
            };
            // Dropping the timed-out setup drops the connection, which closes it.
            tokio::time::timeout(SETUP_TIMEOUT, setup)
                .await
                .map_err(|_| closed("setup timed out"))?
        })
    }
}

/// An authenticated connection to one peer.
pub struct Link {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    peer: Fingerprint,
    local: Fingerprint,
}

impl Link {
    fn new(conn: Connection, send: SendStream, recv: RecvStream, local: Fingerprint) -> Result<Self, NetError> {
        let certs = conn
            .peer_identity()
            .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
            .ok_or_else(|| tls("peer presented no certificate"))?;
        let leaf = certs.first().ok_or_else(|| tls("empty certificate chain"))?;
        Ok(Self {
            peer: fingerprint(leaf),
            local,
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

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), NetError> {
        let frame = encode_frame(msg)?;
        self.send.write_all(&frame).await.map_err(closed)
    }

    /// Reads one frame: 4-byte length, limit check, then exactly that many bytes.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T, NetError> {
        let mut len = [0u8; 4];
        self.recv.read_exact(&mut len).await.map_err(closed)?;
        let n = u32::from_be_bytes(len) as usize;
        if n > MAX_FRAME_LEN {
            self.close("frame too large");
            return Err(FrameError::TooLarge(n).into());
        }
        let mut frame = Vec::with_capacity(4 + n);
        frame.extend_from_slice(&len);
        frame.resize(4 + n, 0);
        self.recv.read_exact(&mut frame[4..]).await.map_err(closed)?;
        match overdesk_proto::decode_frame(&frame)? {
            Some((msg, _)) => Ok(msg),
            None => unreachable!("frame is complete by construction"),
        }
    }

    pub fn send_motion(&self, m: &Motion) -> Result<(), NetError> {
        self.conn.send_datagram(encode_motion(m).into()).map_err(closed)
    }

    pub async fn recv_motion(&self) -> Result<Motion, NetError> {
        let d = self.conn.read_datagram().await.map_err(closed)?;
        Ok(decode_motion(&d)?)
    }

    pub fn close(&self, reason: &str) {
        self.conn.close(0u32.into(), reason.as_bytes());
    }

    /// Resolves when the connection is gone (closed, timed out, or lost).
    pub async fn closed(&self) -> String {
        self.conn.closed().await.to_string()
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
}
