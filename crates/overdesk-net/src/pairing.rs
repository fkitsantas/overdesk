// SPDX-License-Identifier: AGPL-3.0-only
//! Device identity and the 6-digit pairing exchange (ARCHITECTURE §7).
//!
//! Commit-then-reveal: the dialer commits to a fresh nonce before it sees the
//! listener's, so a man in the middle cannot grind keys or nonces until both screens
//! show the same code; it gets one guess in a million per attempt. The code also binds
//! both certificate fingerprints exactly as seen on this connection.

use overdesk_proto::{Pairing, PeerId};
use ring::digest::{SHA256, digest};

pub type Fingerprint = [u8; 32];
pub type Nonce = [u8; 32];

/// SHA-256 of a certificate's DER bytes.
pub fn fingerprint(_cert_der: &[u8]) -> Fingerprint {
    todo!()
}

/// A device's stable id: the first 16 bytes of its certificate fingerprint.
pub fn peer_id(_fp: &Fingerprint) -> PeerId {
    todo!()
}

/// The code both users compare, `0..1_000_000`; display with `{:06}`.
pub fn code(
    _dialer_nonce: &Nonce,
    _listener_nonce: &Nonce,
    _dialer_fp: &Fingerprint,
    _listener_fp: &Fingerprint,
) -> u32 {
    todo!()
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    /// Message arrived out of order or from the wrong role.
    Unexpected,
    /// The revealed nonce does not match the commitment.
    BadReveal,
}

/// What the caller does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Send(Pairing),
    /// Send the message, then show the code to the user.
    SendAndShow(Pairing, u32),
    Show(u32),
}

/// The side that opened the connection.
pub struct Dialer {
    nonce: Nonce,
    dialer_fp: Fingerprint,
    listener_fp: Fingerprint,
    done: bool,
}

impl Dialer {
    /// `nonce` must be fresh random bytes for every attempt.
    pub fn new(_nonce: Nonce, _dialer_fp: Fingerprint, _listener_fp: Fingerprint) -> (Self, Pairing) {
        todo!()
    }

    pub fn on_message(&mut self, _msg: &Pairing) -> Result<Step, PairError> {
        todo!()
    }
}

/// The side that accepted the connection.
pub struct Listener {
    nonce: Nonce,
    dialer_fp: Fingerprint,
    listener_fp: Fingerprint,
    commit: Option<[u8; 32]>,
    done: bool,
}

impl Listener {
    /// `nonce` must be fresh random bytes for every attempt.
    pub fn new(_nonce: Nonce, _dialer_fp: Fingerprint, _listener_fp: Fingerprint) -> Self {
        todo!()
    }

    pub fn on_message(&mut self, _msg: &Pairing) -> Result<Step, PairError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DFP: Fingerprint = [0xD1; 32];
    const LFP: Fingerprint = [0x11; 32];

    /// Runs a full exchange and returns (dialer code, listener code).
    fn run(dn: Nonce, ln: Nonce) -> (u32, u32) {
        let (mut d, commit) = Dialer::new(dn, DFP, LFP);
        let mut l = Listener::new(ln, DFP, LFP);
        let Ok(Step::Send(nonce)) = l.on_message(&commit) else {
            panic!("listener must answer commit")
        };
        let Ok(Step::SendAndShow(reveal, dcode)) = d.on_message(&nonce) else {
            panic!("dialer must reveal")
        };
        let Ok(Step::Show(lcode)) = l.on_message(&reveal) else {
            panic!("listener must accept reveal")
        };
        (dcode, lcode)
    }

    #[test]
    fn both_sides_show_the_same_six_digit_code() {
        let (d, l) = run([1; 32], [2; 32]);
        assert_eq!(d, l);
        assert!(d < 1_000_000);
    }

    #[test]
    fn fresh_nonces_give_fresh_codes() {
        assert_ne!(run([1; 32], [2; 32]).0, run([3; 32], [2; 32]).0);
        assert_ne!(run([1; 32], [2; 32]).0, run([1; 32], [4; 32]).0);
    }

    #[test]
    fn code_binds_both_fingerprints_and_roles() {
        let base = code(&[1; 32], &[2; 32], &DFP, &LFP);
        assert_ne!(base, code(&[1; 32], &[2; 32], &[0xEE; 32], &LFP));
        assert_ne!(base, code(&[1; 32], &[2; 32], &DFP, &[0xEE; 32]));
        assert_ne!(base, code(&[1; 32], &[2; 32], &LFP, &DFP), "roles swapped");
    }

    #[test]
    fn reveal_not_matching_commit_is_rejected() {
        let (_d, commit) = Dialer::new([1; 32], DFP, LFP);
        let mut l = Listener::new([2; 32], DFP, LFP);
        l.on_message(&commit).unwrap();
        assert_eq!(
            l.on_message(&Pairing::Reveal { nonce: [9; 32] }),
            Err(PairError::BadReveal)
        );
    }

    #[test]
    fn commit_made_for_another_dialer_fingerprint_is_rejected() {
        // A relay that replays a commit across connections cannot get it accepted.
        let (_d, commit) = Dialer::new([1; 32], [0xEE; 32], LFP);
        let mut l = Listener::new([2; 32], DFP, LFP);
        l.on_message(&commit).unwrap();
        assert_eq!(
            l.on_message(&Pairing::Reveal { nonce: [1; 32] }),
            Err(PairError::BadReveal)
        );
    }

    #[test]
    fn out_of_order_messages_are_rejected() {
        let mut l = Listener::new([2; 32], DFP, LFP);
        assert_eq!(
            l.on_message(&Pairing::Reveal { nonce: [1; 32] }),
            Err(PairError::Unexpected)
        );
        let (mut d, commit) = Dialer::new([1; 32], DFP, LFP);
        assert_eq!(d.on_message(&commit), Err(PairError::Unexpected));
        l.on_message(&commit).unwrap();
        assert_eq!(l.on_message(&commit), Err(PairError::Unexpected), "second commit");
        let Ok(Step::Send(nonce)) = Listener::new([2; 32], DFP, LFP).on_message(&commit) else {
            panic!()
        };
        d.on_message(&nonce).unwrap();
        assert_eq!(d.on_message(&nonce), Err(PairError::Unexpected), "second nonce");
    }

    #[test]
    fn peer_id_is_fingerprint_prefix() {
        let fp = fingerprint(b"certificate bytes");
        assert_eq!(fp, fingerprint(b"certificate bytes"));
        assert_ne!(fp, fingerprint(b"other certificate"));
        assert_eq!(peer_id(&fp).0, fp[..16]);
    }
}
