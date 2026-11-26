// SPDX-License-Identifier: AGPL-3.0-only
//! On-disk device identity and pinned peers.
//!
//! The directory is 0700 on Unix. The certificate (`cert.der`) and the pins
//! (`pins.txt`) are plain files; the private key is held by the OS where it can be: the
//! macOS Keychain, or a Windows DPAPI blob (`key.dpapi`). Other platforms keep `key.der`
//! with mode 0600. A `key.der` left by an older version is imported into the OS store
//! and deleted, and kept if the import fails.

mod keystore;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::pairing::Fingerprint;
use crate::transport::{Identity, NetError};

/// A directory holding this device's certificate (`cert.der`), its private key (see the
/// module docs for where) and its pinned peers (`pins.txt`, one hex fingerprint per line).
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

/// Rejects bytes that are not a usable PKCS#8 private key, so a damaged key store entry
/// is an error instead of surfacing later as a failed handshake.
///
/// # Errors
///
/// Returns [`NetError::Tls`] if the key cannot be parsed.
fn check_key(key: &[u8]) -> Result<(), NetError> {
    let der = rustls::pki_types::PrivateKeyDer::Pkcs8(key.to_vec().into());
    rustls::crypto::ring::sign::any_supported_type(&der)
        .map(drop)
        .map_err(|e| NetError::Tls(format!("stored device key is unusable: {e}")))
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
    /// If the certificate or the key is missing a new identity is generated, which
    /// changes this device's fingerprint, so every peer must pair again.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Tls`] if generation fails or the stored key is corrupt, and
    /// [`NetError::Io`] if the key store cannot be read or the new identity saved.
    pub fn identity(&self) -> Result<Identity, NetError> {
        let cert_path = self.dir.join("cert.der");
        let key = match keystore::load(&self.dir)? {
            Some(k) => Some(k),
            None => self.migrate_legacy_key(),
        };
        if let (Ok(cert), Some(key)) = (std::fs::read(&cert_path), key) {
            check_key(&key)?;
            return Ok(Identity::from_der(cert, key));
        }
        let id = Identity::generate()?;
        let (cert, key) = id.to_der();
        // Key first: a crash between the writes then leaves no certificate, so the next
        // run regenerates instead of loading a certificate without its key.
        keystore::save(&self.dir, &key)?;
        write_private(&cert_path, &cert)?;
        Ok(id)
    }

    /// Imports a `key.der` from an older version into the OS key store, checks it reads
    /// back, then deletes the file. Returns the legacy key if there was one. On any
    /// failure the file is kept (and the problem logged), so the key is never lost.
    #[cfg(any(target_os = "macos", windows))]
    fn migrate_legacy_key(&self) -> Option<Vec<u8>> {
        let path = self.dir.join("key.der");
        let key = std::fs::read(&path).ok()?;
        let stored = keystore::save(&self.dir, &key).and_then(|()| keystore::load(&self.dir));
        match stored {
            Ok(Some(back)) if back == key => {
                if let Err(e) = std::fs::remove_file(&path) {
                    eprintln!("overdesk: key migrated but key.der not removed: {e}");
                }
            }
            Ok(_) => eprintln!("overdesk: key import did not read back; keeping key.der"),
            Err(e) => eprintln!("overdesk: key import failed, keeping key.der: {e}"),
        }
        Some(key)
    }

    /// No legacy file exists where `key.der` is the live store.
    #[cfg(not(any(target_os = "macos", windows)))]
    #[expect(clippy::unused_self, reason = "same signature as the migrating variant")]
    fn migrate_legacy_key(&self) -> Option<Vec<u8>> {
        None
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

    /// A fresh, unique store path; also the Keychain account on macOS.
    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("overdesk-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Removes the protected key entry and the directory.
    fn cleanup(dir: PathBuf) {
        keystore::delete(&dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn identity_survives_restart() {
        let dir = temp_dir("id");
        let first = Store::open(&dir).unwrap().identity().unwrap().fingerprint();
        let again = Store::open(&dir).unwrap().identity().unwrap().fingerprint();
        assert_eq!(first, again);
        cleanup(dir);
    }

    #[cfg(unix)]
    #[test]
    fn directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perm");
        Store::open(&dir).unwrap().identity().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(mode(&dir.join("key.der")), 0o600);
        cleanup(dir);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn legacy_key_file_migrates_and_is_removed() {
        let dir = temp_dir("legacy");
        let id = Identity::generate().unwrap();
        let (cert, key) = id.to_der();
        let s = Store::open(&dir).unwrap();
        std::fs::write(dir.join("cert.der"), cert).unwrap();
        std::fs::write(dir.join("key.der"), &key).unwrap();
        assert_eq!(s.identity().unwrap().fingerprint(), id.fingerprint());
        assert!(!dir.join("key.der").exists());
        assert_eq!(keystore::load(&dir).unwrap(), Some(key));
        cleanup(dir);
    }

    #[test]
    fn corrupt_key_entry_is_an_error_not_a_panic() {
        let dir = temp_dir("badkey");
        let s = Store::open(&dir).unwrap();
        s.identity().unwrap();
        keystore::save(&dir, b"junk").unwrap();
        assert!(s.identity().is_err());
        cleanup(dir);
    }

    #[cfg(windows)]
    #[test]
    fn corrupt_dpapi_blob_is_an_error_not_a_panic() {
        let dir = temp_dir("baddpapi");
        let s = Store::open(&dir).unwrap();
        s.identity().unwrap();
        std::fs::write(dir.join("key.dpapi"), b"not a blob").unwrap();
        assert!(s.identity().is_err());
        cleanup(dir);
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
