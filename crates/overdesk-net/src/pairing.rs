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
pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    sha256(&[cert_der])
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let bytes = parts.concat();
    digest(&SHA256, &bytes)
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

/// Domain-separated so a commitment can never be reused as a code input.
fn commitment(nonce: &Nonce, dialer_fp: &Fingerprint) -> [u8; 32] {
    sha256(&[b"overdesk-pair-commit-v1", dialer_fp, nonce])
}

/// A device's stable id: the first 16 bytes of its certificate fingerprint.
pub fn peer_id(fp: &Fingerprint) -> PeerId {
    PeerId(fp[..16].try_into().expect("16 of 32 bytes"))
}

/// The code both users compare, `0..1_000_000`; display with `{:06}`.
pub fn code(dialer_nonce: &Nonce, listener_nonce: &Nonce, dialer_fp: &Fingerprint, listener_fp: &Fingerprint) -> u32 {
    let h = sha256(&[
        b"overdesk-pair-code-v1",
        dialer_fp,
        listener_fp,
        dialer_nonce,
        listener_nonce,
    ]);
    u32::from_be_bytes([h[0], h[1], h[2], h[3]]) % 1_000_000
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
    pub fn new(nonce: Nonce, dialer_fp: Fingerprint, listener_fp: Fingerprint) -> (Self, Pairing) {
        let commit = Pairing::Commit {
            digest: commitment(&nonce, &dialer_fp),
        };
        (
            Self {
                nonce,
                dialer_fp,
                listener_fp,
                done: false,
            },
            commit,
        )
    }

    pub fn on_message(&mut self, msg: &Pairing) -> Result<Step, PairError> {
        match msg {
            Pairing::Nonce { nonce: theirs } if !self.done => {
                self.done = true;
                let shown = code(&self.nonce, theirs, &self.dialer_fp, &self.listener_fp);
                Ok(Step::SendAndShow(Pairing::Reveal { nonce: self.nonce }, shown))
            }
            _ => Err(PairError::Unexpected),
        }
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
    pub fn new(nonce: Nonce, dialer_fp: Fingerprint, listener_fp: Fingerprint) -> Self {
        Self {
            nonce,
            dialer_fp,
            listener_fp,
            commit: None,
            done: false,
        }
    }

    pub fn on_message(&mut self, msg: &Pairing) -> Result<Step, PairError> {
        match (msg, self.commit) {
            (Pairing::Commit { digest }, None) => {
                self.commit = Some(*digest);
                Ok(Step::Send(Pairing::Nonce { nonce: self.nonce }))
            }
            (Pairing::Reveal { nonce: theirs }, Some(committed)) if !self.done => {
                self.done = true;
                // Not constant-time: the commitment is public, so timing leaks nothing secret.
                if commitment(theirs, &self.dialer_fp) != committed {
                    return Err(PairError::BadReveal);
                }
                Ok(Step::Show(code(
                    theirs,
                    &self.nonce,
                    &self.dialer_fp,
                    &self.listener_fp,
                )))
            }
            _ => Err(PairError::Unexpected),
        }
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
