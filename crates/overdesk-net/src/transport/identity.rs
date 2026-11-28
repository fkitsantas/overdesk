// SPDX-License-Identifier: AGPL-3.0-only
//! This device's certificate and key, and the verifier that leaves trust to the pinned fingerprint.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use super::error::{NetError, tls};
use crate::pairing::{Fingerprint, fingerprint};

/// TLS server name; certificates are pinned by fingerprint, never matched by name.
pub(super) const SERVER_NAME: &str = "overdesk";

/// This device's long-lived certificate and private key.
///
/// The certificate is self-signed; peers trust it only through the fingerprint pinned
/// at pairing time. The private key never leaves this struct except via the store.
pub struct Identity {
    /// Self-signed X.509 certificate in DER form, presented to every peer.
    pub(super) cert: CertificateDer<'static>,
    /// PKCS#8 private key matching `cert`; it signs the TLS handshake, which proves to
    /// the peer that this device owns the certificate.
    key: PrivatePkcs8KeyDer<'static>,
}

impl Identity {
    /// Generates a fresh self-signed certificate and key pair for this device.
    ///
    /// This only generates; [`crate::store::Store::identity`] persists the key through the
    /// OS key store (Keychain, DPAPI, Secret Service, or a 0600 file).
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
    pub(super) fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

/// Skips certificate-chain validation only: Overdesk trusts by pinned fingerprint,
/// checked after the handshake. Handshake signatures are always verified, so a peer
/// must hold the private key of the certificate it presents.
#[derive(Debug)]
pub(super) struct PinLater(pub(super) Arc<CryptoProvider>);

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
