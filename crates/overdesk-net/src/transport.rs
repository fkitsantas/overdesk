// SPDX-License-Identifier: AGPL-3.0-only
//! One QUIC connection per peer pair: a reliable control stream plus motion datagrams,
//! over mutual TLS 1.3 with self-signed device certificates.

use std::net::SocketAddr;

use overdesk_proto::{FrameError, Motion};
use serde::{Serialize, de::DeserializeOwned};

use crate::pairing::Fingerprint;

pub const ALPN: &[u8] = b"overdesk/1";

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

/// This device's long-lived certificate and private key.
pub struct Identity {
    _todo: (),
}

impl Identity {
    pub fn generate() -> Result<Self, NetError> {
        todo!()
    }

    pub fn fingerprint(&self) -> Fingerprint {
        todo!()
    }
}

/// A bound QUIC endpoint that can both dial and accept.
pub struct Node {
    _todo: (),
}

impl Node {
    pub fn bind(_identity: &Identity, _addr: SocketAddr) -> Result<Self, NetError> {
        todo!()
    }

    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        todo!()
    }

    /// Dials `addr` and opens the control stream.
    pub async fn connect(&self, _addr: SocketAddr) -> Result<Link, NetError> {
        todo!()
    }

    /// Waits for the next peer and its control stream. `None` once the endpoint is closed.
    pub async fn accept(&self) -> Option<Result<Link, NetError>> {
        todo!()
    }
}

/// An authenticated connection to one peer.
pub struct Link {
    _todo: (),
}

impl Link {
    /// The fingerprint of the certificate the peer proved it holds the key for.
    pub fn peer_fingerprint(&self) -> Fingerprint {
        todo!()
    }

    pub async fn send<T: Serialize>(&mut self, _msg: &T) -> Result<(), NetError> {
        todo!()
    }

    /// Reads one frame: 4-byte length, limit check, then exactly that many bytes.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T, NetError> {
        todo!()
    }

    pub fn send_motion(&self, _m: &Motion) -> Result<(), NetError> {
        todo!()
    }

    pub async fn recv_motion(&self) -> Result<Motion, NetError> {
        todo!()
    }

    pub fn close(&self, _reason: &str) {
        todo!()
    }

    /// Resolves when the connection is gone (closed, timed out, or lost).
    pub async fn closed(&self) -> String {
        todo!()
    }
}
