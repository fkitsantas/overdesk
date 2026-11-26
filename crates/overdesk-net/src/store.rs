// SPDX-License-Identifier: AGPL-3.0-only
//! On-disk device identity and pinned peers.
//!
//! shortcut: plain files with 0600 permissions in a 0700 directory; move the private
//! key to the macOS Keychain / Windows DPAPI before shipping installers.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::pairing::Fingerprint;
use crate::transport::{Identity, NetError};

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Opens (creating if needed) the store directory.
    pub fn open(_dir: impl Into<PathBuf>) -> Result<Self, NetError> {
        todo!()
    }

    /// Loads this device's identity, generating and saving one on first run.
    pub fn identity(&self) -> Result<Identity, NetError> {
        todo!()
    }

    pub fn pins(&self) -> Result<BTreeSet<Fingerprint>, NetError> {
        todo!()
    }

    pub fn pin(&self, _fp: Fingerprint) -> Result<(), NetError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("overdesk-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn identity_survives_restart() {
        let dir = temp_dir("id");
        let first = Store::open(&dir).unwrap().identity().unwrap().fingerprint();
        let again = Store::open(&dir).unwrap().identity().unwrap().fingerprint();
        assert_eq!(first, again);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_key_and_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perm");
        Store::open(&dir).unwrap().identity().unwrap();
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(dir.join("key.der")), 0o600);
        assert_eq!(mode(dir.clone()), 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pins_round_trip_and_dedupe() {
        let dir = temp_dir("pins");
        let s = Store::open(&dir).unwrap();
        assert!(s.pins().unwrap().is_empty());
        s.pin([7; 32]).unwrap();
        s.pin([7; 32]).unwrap();
        s.pin([9; 32]).unwrap();
        assert_eq!(Store::open(&dir).unwrap().pins().unwrap(), BTreeSet::from([[7; 32], [9; 32]]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_pins_file_is_an_error_not_a_panic() {
        let dir = temp_dir("corrupt");
        let s = Store::open(&dir).unwrap();
        std::fs::write(dir.join("pins.txt"), "not hex\n").unwrap();
        assert!(s.pins().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
