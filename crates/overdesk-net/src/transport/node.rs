// SPDX-License-Identifier: AGPL-3.0-only
//! The QUIC endpoint: dialing, accepting, and the caps on handshakes in flight.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Endpoint, TransportConfig};

use overdesk_proto::{LEN_PREFIX, MAX_FRAME_LEN};

use super::error::{NetError, closed, io, tls};
use super::identity::{Identity, PinLater, SERVER_NAME};
use super::link::{Link, Role};
use crate::pairing::Fingerprint;
use crate::sync::Relock;

/// ALPN protocol identifier negotiated in the TLS handshake. Both sides offer only this
/// value, so a connection from any other QUIC application fails during the handshake
/// instead of reaching the Overdesk framing layer.
pub const ALPN: &[u8] = b"overdesk/1";
/// Interval between QUIC keep-alive packets, so an idle but healthy link never reaches
/// [`IDLE_TIMEOUT`].
const KEEP_ALIVE: Duration = Duration::from_secs(1);
/// Time without any packet from the peer after which the connection is declared lost: a
/// vanished peer (crash, sleep, cable) is noticed within ~3 s, so its held keys are
/// released and swallowed input comes back quickly.
const IDLE_TIMEOUT: Duration = Duration::from_secs(3);
/// First byte on the control stream. QUIC only announces a stream to the peer once data
/// is written, so the dialer sends this immediately; it doubles as a stream-format version.
const STREAM_PREAMBLE: u8 = 1;
/// A peer that connects but never opens its control stream is dropped after this.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// quinn's bound on incoming connections it has received but we have not yet accepted
/// or refused. It does not bound handshakes in flight once accepted; see
/// [`MAX_HANDSHAKES`].
const MAX_INCOMING: usize = 64;
/// Upper bound on handshakes in flight on one endpoint, from accepting the connection
/// until its [`Link`] exists; further ones are refused, so a burst of connections cannot
/// exhaust memory before any of them is authenticated.
const MAX_HANDSHAKES: usize = 16;
/// Handshakes in flight allowed from one source IP, so one host cannot fill
/// [`MAX_HANDSHAKES`] and lock out the device the user is pairing.
const MAX_HANDSHAKES_PER_IP: usize = 4;

/// QUIC transport settings shared by the server and client sides: liveness timers,
/// tight flow-control windows and small datagram buffers, sized for one control stream
/// and a trickle of motion datagrams. With `transfers`, the peer may also open one
/// inbound uni stream (see [`LinkTx::open_transfer`]) and the connection window grows to
/// four frames, so a bulk transfer filling its own two-frame stream window still leaves
/// room for control frames.
fn transport(transfers: bool) -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("3 s fits a QUIC varint")));
    // One control stream per connection and windows just above one frame: quinn's
    // defaults (100+100 streams, unbounded connection window) let any peer that merely
    // completes TLS make us buffer hundreds of megabytes.
    let frame = (MAX_FRAME_LEN + LEN_PREFIX) as u32;
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(u32::from(transfers).into());
    t.stream_receive_window((2 * frame).into());
    // Small until `Link::trust` raises it (see `PRE_TRUST_WINDOW`). The stream window cannot be
    // raised after the handshake, so it keeps its full size; the connection window bounds both.
    t.receive_window(PRE_TRUST_WINDOW.into());
    t.send_window(4 * frame as u64);
    // Motion datagrams are ~15 bytes; a small buffer drops stale ones under load.
    t.datagram_receive_buffer_size(Some(16 * 1024));
    // Small on purpose: a queued motion sample older than the next one is worse than none.
    t.datagram_send_buffer_size(512);
    Arc::new(t)
}

/// The connection receive window until the peer is trusted: room for pairing messages (at most
/// 128 bytes each) and a `Hello`, nothing more, so a peer that has merely completed TLS can make
/// us buffer a few KB, not megabytes. [`Link::trust`] raises it to [`trusted_window`].
pub(super) const PRE_TRUST_WINDOW: u32 = 8 * 1024;

/// The connection receive window of a trusted peer: two frames, four with transfers.
pub(super) fn trusted_window(transfers: bool) -> u32 {
    let frame = (MAX_FRAME_LEN + LEN_PREFIX) as u32;
    (if transfers { 4 } else { 2 }) * frame
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
    /// Counts handshakes in flight, overall and per source IP.
    admission: Arc<Admission>,
    /// What a link's connection window becomes once its peer is trusted.
    trusted_window: u32,
}

/// Counts handshakes in flight so [`Node::accept`] can refuse the excess.
#[derive(Default)]
struct Admission {
    /// Handshakes in flight by source address; the total is the sum.
    in_flight: Mutex<HashMap<IpAddr, usize>>,
}

/// One admitted handshake; releases its slot on drop, whether setup finished, failed or
/// was never polled.
struct Slot {
    /// Where the slots are counted.
    admission: Arc<Admission>,
    /// Source address this slot is charged to.
    ip: IpAddr,
}

/// The address a handshake is charged to: IPv4-mapped IPv6 addresses count as their IPv4
/// address, and other IPv6 addresses by their /64, since one host owns a whole /64 and
/// could otherwise rotate through it to dodge the per-source cap.
fn source_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6((u128::from(a) & !0u128 << 64).into()),
        },
    }
}

impl Admission {
    /// Takes a slot for `ip`, or `None` if the endpoint or that address is at its limit.
    fn enter(self: &Arc<Self>, ip: IpAddr, total_max: usize, per_ip_max: usize) -> Option<Slot> {
        let ip = source_key(ip);
        let mut m = self.in_flight.relock();
        if m.values().sum::<usize>() >= total_max || m.get(&ip).copied().unwrap_or(0) >= per_ip_max {
            return None;
        }
        *m.entry(ip).or_insert(0) += 1;
        Some(Slot {
            admission: self.clone(),
            ip,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut m = self
            .admission
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
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
        Self::bind_with_transfers(identity, addr, false)
    }

    /// Like [`Node::bind`], and with `transfers` the peer may open one transfer stream
    /// at a time to this node ([`TransferRx::accept`](super::TransferRx::accept)). Off in [`Node::bind`]: such a
    /// node resets every uni stream, which keeps Free limits exactly.
    ///
    /// # Errors
    ///
    /// Same as [`Node::bind`].
    pub fn bind_with_transfers(identity: &Identity, addr: SocketAddr, transfers: bool) -> Result<Self, NetError> {
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
        server.transport_config(transport(transfers));
        // A desk has a handful of devices; bound the queue of not-yet-accepted connections
        // here and the accepted handshakes in `Node::accept`.
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
        client.transport_config(transport(transfers));

        let mut endpoint = Endpoint::server(server, addr).map_err(io)?;
        endpoint.set_default_client_config(client);
        Ok(Self {
            endpoint,
            local: identity.fingerprint(),
            admission: Arc::default(),
            trusted_window: trusted_window(transfers),
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
        let setup = async {
            let conn = self
                .endpoint
                .connect(addr, SERVER_NAME)
                .map_err(io)?
                .await
                .map_err(closed)?;
            let (mut send, recv) = conn.open_bi().await.map_err(closed)?;
            // Without this write the listener would never see the stream and would time out.
            send.write_all(&[STREAM_PREAMBLE]).await.map_err(closed)?;
            Link::new(conn, send, recv, self.local, Role::Dialer, self.trusted_window)
        };
        // A peer that stalls at any step must not hang the caller; dropping closes the connection.
        tokio::time::timeout(SETUP_TIMEOUT, setup)
            .await
            .map_err(|_| closed("setup timed out"))?
    }

    /// Waits for the next incoming connection and returns its setup as a separate
    /// future, so a stalled peer cannot hold up others: spawn one per connection.
    /// Setup (handshake, control stream, preamble) is bounded by [`SETUP_TIMEOUT`].
    /// At most 16 setups are in flight per node and 4 per source IP; the excess is refused
    /// outright. A slot is held from here until the future finishes (or is dropped), so
    /// spawn it promptly. `None` once the endpoint is closed.
    pub async fn accept(&self) -> Option<impl Future<Output = Result<Link, NetError>> + use<>> {
        let (incoming, slot) = loop {
            let incoming = self.endpoint.accept().await?;
            // Make unvalidated sources prove their address first (QUIC Retry), so spoofed
            // floods cost us a stateless packet instead of a handshake.
            if !incoming.remote_address_validated() && incoming.may_retry() {
                let _ = incoming.retry();
                continue;
            }
            // The address is validated now, so charging it to a per-IP cap is meaningful.
            let Some(slot) =
                self.admission
                    .enter(incoming.remote_address().ip(), MAX_HANDSHAKES, MAX_HANDSHAKES_PER_IP)
            else {
                incoming.refuse();
                continue;
            };
            break (incoming, slot);
        };
        let (local, trusted_window) = (self.local, self.trusted_window);
        Some(async move {
            let _slot = slot;
            let setup = async {
                let conn = incoming.await.map_err(closed)?;
                let (send, mut recv) = conn.accept_bi().await.map_err(closed)?;
                let mut preamble = [0u8];
                recv.read_exact(&mut preamble).await.map_err(closed)?;
                if preamble[0] != STREAM_PREAMBLE {
                    conn.close(1u32.into(), b"unknown stream format");
                    return Err(closed("unknown stream format"));
                }
                Link::new(conn, send, recv, local, Role::Listener, trusted_window)
            };
            // Dropping the timed-out setup drops the connection, which closes it.
            tokio::time::timeout(SETUP_TIMEOUT, setup)
                .await
                .map_err(|_| closed("setup timed out"))?
        })
    }
}

/// A bound node on the loopback interface with a fresh identity, for the tests of every
/// transport module.
#[cfg(test)]
pub(super) fn node() -> (Identity, Node) {
    let id = Identity::generate().unwrap();
    let node = Node::bind(&id, "127.0.0.1:0".parse().unwrap()).unwrap();
    (id, node)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts the next connection and runs its setup in the background, as a real caller does.
    async fn spawn_next_setup(server: &Node) -> tokio::task::JoinHandle<Result<Link, NetError>> {
        tokio::spawn(server.accept().await.unwrap())
    }

    /// Completes TLS but never opens the control stream.
    async fn stalled_peer(addr: SocketAddr) -> (Node, quinn::Connection) {
        let (_, n) = node();
        let conn = n.endpoint.connect(addr, SERVER_NAME).unwrap().await.unwrap();
        (n, conn)
    }

    #[test]
    fn source_key_groups_a_slash_64_and_unmaps_ipv4() {
        let k = |s: &str| source_key(s.parse().unwrap());
        assert_eq!(k("2001:db8:1:2::1"), k("2001:db8:1:2:ffff:ffff:ffff:ffff"));
        assert_ne!(k("2001:db8:1:2::1"), k("2001:db8:1:3::1"));
        assert_eq!(k("::ffff:10.0.0.1"), k("10.0.0.1"));
        let adm = Arc::new(Admission::default());
        let a: IpAddr = "2001:db8::1".parse().unwrap();
        let b: IpAddr = "2001:db8::2".parse().unwrap();
        let _s = adm.enter(a, 9, 1).unwrap();
        assert!(adm.enter(b, 9, 1).is_none());
    }

    #[test]
    fn admission_caps_total_and_per_source() {
        let adm = Arc::new(Admission::default());
        let (a, b): (IpAddr, IpAddr) = ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap());
        let held: Vec<_> = (0..2).map(|_| adm.enter(a, 3, 2).unwrap()).collect();
        assert!(adm.enter(a, 3, 2).is_none(), "per-IP cap");
        let _b = adm.enter(b, 3, 2).unwrap();
        assert!(adm.enter(b, 3, 2).is_none(), "total cap");
        drop(held);
        assert!(adm.enter(a, 3, 2).is_some(), "slots free on drop");
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
    async fn connect_to_a_silent_listener_times_out() {
        let (_, server) = node();
        let addr = server.local_addr().unwrap();
        let (_, client) = node();
        // The server never calls accept, so the handshake never completes.
        let started = std::time::Instant::now();
        let result = client.connect(addr).await;
        assert!(matches!(result, Err(NetError::Closed(_))));
        assert!(started.elapsed() < SETUP_TIMEOUT + Duration::from_secs(2));
    }
}
