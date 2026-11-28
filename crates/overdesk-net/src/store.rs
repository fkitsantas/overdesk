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

use crate::hex::{hex, unhex};
use crate::pairing::Fingerprint;
use crate::sync::Relock;
use crate::transport::{Identity, NetError, io};

/// A directory holding this device's certificate (`cert.der`), its private key (see the
/// module docs for where) and its pinned peers (`pins.txt`, one hex fingerprint per line).
///
/// Anyone who can read the directory can impersonate this device, and anyone who can
/// write it can add a trusted peer, so it is kept owner-only.
pub struct Store {
    /// Canonical path of the store directory; it names the OS key store entry.
    dir: PathBuf,
    /// The directory exactly as the caller named it. Builds before canonicalisation keyed
    /// the OS key store entry on this, so a key saved through a symlinked path (`/tmp` on
    /// macOS, a linked home) is still found and moved to the canonical name.
    given: PathBuf,
    /// Serialises the read-modify-write of `pins.txt` between threads sharing this store.
    pin_lock: std::sync::Mutex<()>,
    /// What the key store worked around since the last [`Store::take_warnings`].
    warnings: std::sync::Mutex<Vec<StoreWarning>>,
}

/// Something the key store coped with that its user may want to know about: a legacy `key.der`
/// that could not be cleaned up, or an import that was not completed. The store carries on (the
/// key is never lost), so these are reported through [`Store::take_warnings`], not as errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreWarning {
    /// A `key.der` that only duplicates the key in the OS store could not be deleted; the next
    /// load tries again.
    #[error("stale key.der not removed, will retry: {0}")]
    StaleKeyFile(String),
    /// The key was moved into the OS store but the old `key.der` could not be deleted.
    #[error("key migrated but key.der not removed: {0}")]
    MigratedKeyFileKept(String),
    /// `key.der` is not the key of `cert.der`, so it was not imported.
    #[error("key.der does not match cert.der; not importing it")]
    KeyFileMismatch,
    /// The OS store accepted the key but did not give it back; `key.der` is kept.
    #[error("key import did not read back; keeping key.der")]
    ImportNotReadBack,
    /// The OS store refused the key; `key.der` is kept.
    #[error("key import failed, keeping key.der: {0}")]
    ImportFailed(String),
}

/// Writes via a uniquely named temp file and rename, so a crash mid-write never leaves a
/// half file and concurrent writers never share a temp file. On Unix the file is created
/// with mode 0600, so the key is never briefly readable by others, and the directory is
/// synced after the rename so the new name survives a power cut. The temp file is removed
/// on failure.
///
/// # Errors
///
/// Returns [`NetError::Io`] if the file cannot be written, synced or renamed.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), NetError> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let written = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp).map_err(io)?;
        std::io::Write::write_all(&mut f, bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return written;
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent).and_then(|d| d.sync_all()).map_err(io)?;
    }
    Ok(())
}

/// Accepts only names that are safe as a file name and a key store attribute.
fn check_secret_name(name: &str) -> Result<(), NetError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(io(format!("bad secret name: {name:?}")))
    }
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

/// Whether `key` is the private half of `cert`: the key's public key (SPKI) must appear in
/// the certificate. A key whose public half cannot be derived is given the benefit of the
/// doubt, since [`check_key`] already vouches it parses.
fn key_matches_cert(cert: &[u8], key: &[u8]) -> bool {
    let der = rustls::pki_types::PrivateKeyDer::Pkcs8(key.to_vec().into());
    let Ok(signer) = rustls::crypto::ring::sign::any_supported_type(&der) else {
        return false;
    };
    signer
        .public_key()
        .is_none_or(|spki| cert.windows(spki.as_ref().len()).any(|w| w == spki.as_ref()))
}

impl Store {
    /// Opens (creating if needed) the store directory, owner-only.
    ///
    /// The path is canonicalised once the directory exists, so the Keychain account and
    /// Secret Service attribute do not change with how the caller spelled the path
    /// (relative, symlinked, trailing slash).
    ///
    /// On Unix the directory mode is reset to 0700 every time, so loosened permissions
    /// are tightened again.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Io`] if the directory cannot be created or its permissions
    /// set.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, NetError> {
        let given = dir.into();
        std::fs::create_dir_all(&given).map_err(io)?;
        let dir = std::fs::canonicalize(&given).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        Ok(Self {
            dir,
            given,
            pin_lock: std::sync::Mutex::new(()),
            warnings: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Loads this device's identity, generating and saving one on first run.
    ///
    /// Generation happens only when there is no `cert.der` at all. A certificate whose key
    /// cannot be found (a locked or unreachable keyring, a deleted entry) is an error and
    /// is never overwritten: regenerating would change this device's fingerprint, force
    /// every peer to pair again, and leave a key that no longer matches the old pins.
    ///
    /// This does blocking file and OS key store I/O (on Linux it talks to the Secret
    /// Service on a helper thread); call it at startup or from `spawn_blocking`, not on
    /// a runtime worker.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Tls`] if generation fails or the stored key is corrupt, and
    /// [`NetError::Io`] if the certificate or key store cannot be read, the certificate
    /// exists without a key, or the new identity cannot be saved.
    pub fn identity(&self) -> Result<Identity, NetError> {
        let mut warnings = self.warnings.relock();
        let w = &mut *warnings;
        let cert_path = self.dir.join("cert.der");
        let cert = match std::fs::read(&cert_path) {
            Ok(c) => Some(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io(e)),
        };
        let key = match keystore::load(&self.dir, None, w)? {
            Some(k) => {
                self.drop_stale_key_file(&k, w);
                Some(k)
            }
            None => self.migrate_given_name(w)?.or_else(|| self.migrate_legacy_key(w)),
        };
        match (cert, key) {
            (Some(cert), Some(key)) => {
                check_key(&key)?;
                if !key_matches_cert(&cert, &key) {
                    return Err(NetError::Tls("stored device key does not match cert.der".into()));
                }
                Ok(Identity::from_der(cert, key))
            }
            (Some(_), None) => Err(io(
                "cert.der exists but its private key was not found (keyring locked or unreachable, or the \
                 entry was removed); not regenerating, as that would orphan every pairing",
            )),
            (None, _) => {
                let id = Identity::generate()?;
                let (cert, key) = id.to_der();
                // Key first: a crash between the writes then leaves no certificate, so the
                // next run regenerates instead of loading a certificate without its key.
                keystore::save(&self.dir, None, &key, w)?;
                write_private(&cert_path, &cert)?;
                Ok(id)
            }
        }
    }

    /// Deletes a `key.der` that only duplicates the key the OS store holds (left by an
    /// interrupted migration). A failed delete is reported and retried on the next load.
    #[cfg(any(target_os = "macos", windows))]
    fn drop_stale_key_file(&self, key: &[u8], w: &mut Vec<StoreWarning>) {
        if std::fs::read(self.dir.join("key.der")).is_ok_and(|f| f == key) {
            keystore::remove_stale_key_file(&self.dir, w);
        }
    }

    /// Elsewhere `key.der` is the live store (or, on Linux, handled by the keystore).
    #[cfg(not(any(target_os = "macos", windows)))]
    #[expect(clippy::unused_self, reason = "same signature as the cleaning variant")]
    fn drop_stale_key_file(&self, _key: &[u8], _w: &mut Vec<StoreWarning>) {}

    /// A key saved by an older build under the directory name as given (not canonical):
    /// copied to the canonical entry and returned. The old entry is left in place, since a
    /// downgrade would still look there. `None` when the names agree or nothing is there.
    fn migrate_given_name(&self, w: &mut Vec<StoreWarning>) -> Result<Option<Vec<u8>>, NetError> {
        if self.given == self.dir {
            return Ok(None);
        }
        let Some(key) = keystore::load(&self.given, None, w)? else {
            return Ok(None);
        };
        keystore::save(&self.dir, None, &key, w)?;
        Ok(Some(key))
    }

    /// Imports a `key.der` from an older version into the OS key store, checks it reads
    /// back, then deletes the file. Returns the legacy key if there was one. On any
    /// failure the file is kept (and the problem reported), so the key is never lost.
    #[cfg(any(target_os = "macos", windows))]
    fn migrate_legacy_key(&self, w: &mut Vec<StoreWarning>) -> Option<Vec<u8>> {
        let path = self.dir.join("key.der");
        let key = std::fs::read(&path).ok()?;
        if let Ok(cert) = std::fs::read(self.dir.join("cert.der"))
            && !key_matches_cert(&cert, &key)
        {
            w.push(StoreWarning::KeyFileMismatch);
            return Some(key);
        }
        let stored = keystore::save(&self.dir, None, &key, w)
            .and_then(|()| keystore::load(&self.dir, None, w))
            .map_err(|e| e.to_string());
        keystore::finish_import(&self.dir, &key, stored, w);
        Some(key)
    }

    /// Nothing to import here: on Linux the keystore migrates `key.der` itself, elsewhere
    /// the file is the live store.
    #[cfg(not(any(target_os = "macos", windows)))]
    #[expect(clippy::unused_self, reason = "same signature as the migrating variant")]
    fn migrate_legacy_key(&self, _w: &mut Vec<StoreWarning>) -> Option<Vec<u8>> {
        None
    }

    /// Keeps `secret` under `name` in the same protected place as the device key: the macOS
    /// Keychain, a Windows DPAPI blob, the Linux Secret Service, else a 0600 file. It replaces
    /// any earlier secret of that name. For small secrets other Overdesk modules own (a key seed);
    /// the device key itself is [`Store::identity`]'s.
    ///
    /// `name` is 1 to 64 characters of `a-z`, `0-9`, `-` and `_`, as it becomes part of a file name.
    /// Blocking I/O, as [`Store::identity`].
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] if `name` is not allowed or the secret cannot be protected or written.
    pub fn save_secret(&self, name: &str, secret: &[u8]) -> Result<(), NetError> {
        check_secret_name(name)?;
        keystore::save(&self.dir, Some(name), secret, &mut self.warnings.relock())
    }

    /// The secret kept under `name`, or `None` if there is none (including a locked or
    /// unreachable Linux keyring with no file). Blocking I/O.
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] if `name` is not allowed or the store cannot be read.
    pub fn load_secret(&self, name: &str) -> Result<Option<Vec<u8>>, NetError> {
        check_secret_name(name)?;
        keystore::load(&self.dir, Some(name), &mut self.warnings.relock())
    }

    /// Forgets the secret kept under `name`; there being none is not an error. Blocking I/O.
    ///
    /// # Errors
    ///
    /// [`NetError::Io`] if `name` is not allowed or the store refuses the removal.
    pub fn delete_secret(&self, name: &str) -> Result<(), NetError> {
        check_secret_name(name)?;
        keystore::delete(&self.dir, Some(name))
    }

    /// Hands over, and forgets, what the key store worked around in [`Store::identity`] and
    /// the secret calls since the last call. Log them (`overdesk_proto::log::warn`) once the
    /// call that caused them has returned.
    pub fn take_warnings(&self) -> Vec<StoreWarning> {
        std::mem::take(&mut *self.warnings.relock())
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

    /// Rewrites `pins.txt` atomically with `pins`, one hex fingerprint per line.
    fn write_pins(&self, pins: &BTreeSet<Fingerprint>) -> Result<(), NetError> {
        let text: String = pins.iter().map(|p| hex(p) + "\n").collect();
        write_private(&self.dir.join("pins.txt"), text.as_bytes())
    }

    /// Adds `fp` to the pinned peers and rewrites the file atomically. Pinning an
    /// existing fingerprint is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Io`] if the current pins cannot be read or the file cannot
    /// be written.
    pub fn pin(&self, fp: Fingerprint) -> Result<(), NetError> {
        let _guard = self.pin_lock.relock();
        let mut pins = self.pins()?;
        pins.insert(fp);
        self.write_pins(&pins)
    }

    /// Removes `fp` from the pinned peers and rewrites the file atomically. Unpinning a
    /// fingerprint that is not pinned is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::Io`] if the current pins cannot be read or the file cannot
    /// be written.
    pub fn unpin(&self, fp: Fingerprint) -> Result<(), NetError> {
        let _guard = self.pin_lock.relock();
        let mut pins = self.pins()?;
        if !pins.remove(&fp) {
            return Ok(());
        }
        self.write_pins(&pins)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, unique store path; also the Keychain account on macOS.
    fn temp_dir(name: &str) -> PathBuf {
        // Canonical, because `Store::open` canonicalises and the key store entry is named by it.
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let d = base.join(format!("overdesk-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A delete must succeed, except on Linux, where it reports a missing Secret Service (a
    /// headless box) as an error even though the file store is gone.
    fn removed(r: Result<(), NetError>) {
        if cfg!(not(free_unix)) {
            r.unwrap();
        }
    }

    /// Removes the protected key entry and the directory.
    fn cleanup(dir: PathBuf) {
        removed(keystore::delete(&dir, None));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn secrets_round_trip_replace_and_delete_apart_from_the_identity() {
        let dir = temp_dir("secret");
        let store = Store::open(&dir).unwrap();
        let fp = store.identity().unwrap().fingerprint();
        assert_eq!(store.load_secret("seed-a").unwrap(), None);
        store.save_secret("seed-a", b"one").unwrap();
        store.save_secret("seed-b", b"two").unwrap();
        store.save_secret("seed-a", b"three").unwrap();
        assert_eq!(store.load_secret("seed-a").unwrap().as_deref(), Some(&b"three"[..]));
        assert_eq!(store.load_secret("seed-b").unwrap().as_deref(), Some(&b"two"[..]));
        // Reopened (as after a restart) and next to an untouched identity.
        let again = Store::open(&dir).unwrap();
        assert_eq!(again.load_secret("seed-b").unwrap().as_deref(), Some(&b"two"[..]));
        assert_eq!(again.identity().unwrap().fingerprint(), fp);
        removed(again.delete_secret("seed-a"));
        removed(again.delete_secret("seed-a")); // already gone: fine
        assert_eq!(again.load_secret("seed-a").unwrap(), None);
        assert_eq!(again.load_secret("seed-b").unwrap().as_deref(), Some(&b"two"[..]));
        removed(again.delete_secret("seed-b"));
        cleanup(dir);
    }

    #[test]
    fn unpin_removes_only_that_fingerprint() {
        let dir = temp_dir("unpin");
        let store = Store::open(&dir).unwrap();
        store.pin([1; 32]).unwrap();
        store.pin([2; 32]).unwrap();
        store.unpin([3; 32]).unwrap(); // not pinned: fine
        store.unpin([1; 32]).unwrap();
        assert_eq!(store.pins().unwrap(), BTreeSet::from([[2; 32]]));
        store.unpin([2; 32]).unwrap();
        assert!(store.pins().unwrap().is_empty());
        cleanup(dir);
    }

    #[test]
    fn secret_names_cannot_escape_the_directory() {
        let dir = temp_dir("secret-name");
        let store = Store::open(&dir).unwrap();
        for bad in ["", "../x", "a/b", "A", "x.y", &"n".repeat(65)] {
            assert!(store.save_secret(bad, b"x").is_err(), "{bad:?}");
            assert!(store.load_secret(bad).is_err());
            assert!(store.delete_secret(bad).is_err());
        }
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
        // Only the file backend keeps key.der: the OS stores hold the key elsewhere.
        #[cfg(not(any(target_os = "macos", windows)))]
        if dir.join("key.der").exists() {
            assert_eq!(mode(&dir.join("key.der")), 0o600);
        }
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
        assert_eq!(keystore::load(&dir, None, &mut Vec::new()).unwrap(), Some(key));
        cleanup(dir);
    }

    #[test]
    fn cert_without_its_key_is_an_error_and_is_never_regenerated() {
        let dir = temp_dir("nokey");
        let s = Store::open(&dir).unwrap();
        s.identity().unwrap();
        let cert = std::fs::read(s.dir.join("cert.der")).unwrap();
        removed(keystore::delete(&s.dir, None));
        assert!(s.identity().is_err());
        assert_eq!(std::fs::read(s.dir.join("cert.der")).unwrap(), cert);
        cleanup(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_key_saved_under_a_symlinked_name_is_found_and_moved() {
        // An older build named the key store entry by the path as given; open through a link.
        let real = temp_dir("linked");
        std::fs::create_dir_all(&real).unwrap();
        let link = real.with_file_name(format!("overdesk-store-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let id = Identity::generate().unwrap();
        let (cert, key) = id.to_der();
        keystore::save(&link, None, &key, &mut Vec::new()).unwrap();
        write_private(&real.join("cert.der"), &cert).unwrap();
        let s = Store::open(&link).unwrap();
        assert_eq!(s.identity().unwrap().fingerprint(), id.fingerprint());
        // Now stored under the canonical name too, so the next start needs no fallback.
        assert_eq!(keystore::load(&s.dir, None, &mut Vec::new()).unwrap(), Some(key));
        removed(keystore::delete(&link, None));
        std::fs::remove_file(&link).unwrap();
        cleanup(real);
    }

    #[test]
    fn a_key_for_another_certificate_is_refused() {
        let dir = temp_dir("mismatch");
        let s = Store::open(&dir).unwrap();
        s.identity().unwrap();
        keystore::save(&s.dir, None, &Identity::generate().unwrap().to_der().1, &mut Vec::new()).unwrap();
        assert!(matches!(s.identity(), Err(NetError::Tls(_))));
        cleanup(dir);
    }

    #[test]
    fn unreadable_cert_is_an_error_not_absent() {
        let dir = temp_dir("certdir");
        let s = Store::open(&dir).unwrap();
        std::fs::create_dir(s.dir.join("cert.der")).unwrap(); // reading a directory fails, not NotFound
        assert!(s.identity().is_err());
        std::fs::remove_dir(s.dir.join("cert.der")).unwrap();
        cleanup(dir);
    }

    #[test]
    fn open_canonicalises_the_directory() {
        let dir = temp_dir("canon");
        let spelled = dir.join("sub").join("..");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let s = Store::open(&spelled).unwrap();
        assert_eq!(s.dir, std::fs::canonicalize(&dir).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn stale_key_file_matching_the_os_store_is_deleted() {
        let dir = temp_dir("stale");
        let s = Store::open(&dir).unwrap();
        s.identity().unwrap();
        let key = keystore::load(&s.dir, None, &mut Vec::new()).unwrap().unwrap();
        std::fs::write(s.dir.join("key.der"), &key).unwrap();
        s.identity().unwrap();
        assert!(!s.dir.join("key.der").exists());
        cleanup(dir);
    }

    #[test]
    fn concurrent_pins_are_all_kept_and_leave_no_temp_files() {
        let dir = temp_dir("racepins");
        let s = std::sync::Arc::new(Store::open(&dir).unwrap());
        let threads: Vec<_> = (0..8u8)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || s.pin([i; 32]).unwrap())
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(s.pins().unwrap().len(), 8);
        assert!(
            std::fs::read_dir(&s.dir)
                .unwrap()
                .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_key_entry_is_an_error_not_a_panic() {
        let dir = temp_dir("badkey");
        let s = Store::open(&dir).unwrap();
        s.identity().unwrap();
        keystore::save(&dir, None, b"junk", &mut Vec::new()).unwrap();
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
