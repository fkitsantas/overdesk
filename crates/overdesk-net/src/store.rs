// SPDX-License-Identifier: AGPL-3.0-only
//! On-disk device identity and pinned peers.
//!
//! shortcut: plain files with 0600 permissions in a 0700 directory; move the private
//! key to the macOS Keychain / Windows DPAPI before shipping installers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::pairing::Fingerprint;
use crate::transport::{Identity, NetError};

/// A directory holding this device's identity (`cert.der`, `key.der`) and its pinned
/// peers (`pins.txt`, one hex fingerprint per line).
///
/// Anyone who can read the directory can impersonate this device, and anyone who can
/// write it can add a trusted peer, so it is kept owner-only.
pub struct Store {
    /// Path of the store directory.
    dir: PathBuf,
}

/// Wraps any displayable error as [`NetError::Io`], for use with `map_err`.
fn io(e: impl std::fmt::Display) -> NetError {
    NetError::Io(e.to_string())
}

/// Writes via a temp file and rename, so a crash mid-write never leaves a half file.
/// On Unix the file is created with mode 0600, so the key is never briefly readable by
/// others.
///
/// # Errors
///
/// Returns [`NetError::Io`] if the file cannot be written, synced or renamed.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), NetError> {
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(&tmp).map_err(io)?;
    std::io::Write::write_all(&mut f, bytes).map_err(io)?;
    f.sync_all().map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// Lower-case hex encoding of a fingerprint: 64 characters.
fn hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses exactly 64 hex characters back into a fingerprint. Returns `None` for any
/// other length, non-ASCII input or non-hex digit, so a malformed line never panics on
/// a slice boundary.
fn unhex(line: &str) -> Option<Fingerprint> {
    if line.len() != 64 || !line.is_ascii() {
        return None;
    }
    let mut fp = [0u8; 32];
    for (i, b) in fp.iter_mut().enumerate() {
        *b = u8::from_str_radix(&line[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(fp)
}

impl Store {
    /// Opens (creating if needed) the store directory, owner-only.
    ///
    /// On Unix the directory mode is reset to 0700 every time, so loosened permissions
    /// are tightened again.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Io`] if the directory cannot be created or its permissions
    /// set.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, NetError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        Ok(Self { dir })
    }

    /// Loads this device's identity, generating and saving one on first run.
    ///
    /// If either file is missing or unreadable a new identity is generated, which
    /// changes this device's fingerprint, so every peer must pair again.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Tls`] if generation fails and [`NetError::Io`] if the new
    /// identity cannot be saved.
    pub fn identity(&self) -> Result<Identity, NetError> {
        let (cert_path, key_path) = (self.dir.join("cert.der"), self.dir.join("key.der"));
        match (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            (Ok(cert), Ok(key)) => Ok(Identity::from_der(cert, key)),
            _ => {
                let id = Identity::generate()?;
                let (cert, key) = id.to_der();
                // Key first: a crash between the writes then leaves no certificate, so
                // the next run regenerates instead of loading a certificate without its key.
                write_private(&key_path, &key)?;
                write_private(&cert_path, &cert)?;
                Ok(id)
            }
        }
    }

    /// The set of pinned peer fingerprints; empty if nothing has been pinned yet.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Io`] if the file cannot be read or any line is not a valid
    /// fingerprint. A corrupt file is reported rather than skipped, so a damaged pin
    /// list never silently changes who is trusted.
    pub fn pins(&self) -> Result<BTreeSet<Fingerprint>, NetError> {
        let text = match std::fs::read_to_string(self.dir.join("pins.txt")) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(e) => return Err(io(e)),
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| unhex(l.trim()).ok_or_else(|| io(format!("bad pin line: {l:?}"))))
            .collect()
    }

    /// Adds `fp` to the pinned peers and rewrites the file atomically. Pinning an
    /// existing fingerprint is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Io`] if the current pins cannot be read or the file cannot
    /// be written.
    pub fn pin(&self, fp: Fingerprint) -> Result<(), NetError> {
        let mut pins = self.pins()?;
        pins.insert(fp);
        let text: String = pins.iter().map(|p| hex(p) + "\n").collect();
        write_private(&self.dir.join("pins.txt"), text.as_bytes())
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
        assert_eq!(
            Store::open(&dir).unwrap().pins().unwrap(),
            BTreeSet::from([[7; 32], [9; 32]])
        );
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
