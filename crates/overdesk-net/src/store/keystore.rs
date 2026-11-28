// SPDX-License-Identifier: AGPL-3.0-only
//! Where the device's private key (PKCS#8 DER) lives.
//!
//! - macOS: a Keychain generic password, service [`SERVICE`], account = store directory.
//! - Windows: a DPAPI blob (current user) in `key.dpapi` inside the store directory.
//! - Linux: the freedesktop Secret Service (GNOME Keyring, KWallet) item
//!   `service = SERVICE, store = <store directory>`. When no Secret Service is usable
//!   (headless, no session bus, keyring locked) the 0600 `key.der` file is used instead,
//!   and a `key.der` from such a run is moved into the Secret Service on the next load
//!   that finds one (stored, read back, then deleted; the key bytes never change).
//! - Elsewhere: the plain `key.der` file with mode 0600.

use std::path::Path;

use super::StoreWarning;
use crate::transport::NetError;

/// Keychain service name of the device key on macOS.
#[cfg(any(target_os = "macos", free_unix))]
const SERVICE: &str = "org.overdesk.Overdesk.device-key";

/// Service name of the named secrets (`Store::save_secret`), apart from the device key's.
#[cfg(any(target_os = "macos", free_unix))]
const SECRETS: &str = "org.overdesk.Overdesk.secret";

/// Deletes the `key.der` of `dir`, which only duplicates the key the OS store holds. A failed
/// delete (a missing file is fine) is a warning; the next load retries.
#[cfg(any(target_os = "macos", windows, free_unix))]
pub(super) fn remove_stale_key_file(dir: &Path, w: &mut Vec<StoreWarning>) {
    if let Err(e) = std::fs::remove_file(dir.join("key.der"))
        && e.kind() != std::io::ErrorKind::NotFound
    {
        w.push(StoreWarning::StaleKeyFile(e.to_string()));
    }
}

/// Ends the move of a legacy `key.der` into the OS store. `read_back` is what reading the
/// store after saving `key` gave: only the identical key lets the file go, and in every other
/// case the file stays and the reason is a warning, so the key is never lost.
#[cfg(any(target_os = "macos", windows, free_unix))]
pub(super) fn finish_import(
    dir: &Path,
    key: &[u8],
    read_back: Result<Option<Vec<u8>>, String>,
    w: &mut Vec<StoreWarning>,
) {
    match read_back {
        Ok(Some(back)) if back == key => {
            if let Err(e) = std::fs::remove_file(dir.join("key.der")) {
                w.push(StoreWarning::MigratedKeyFileKept(e.to_string()));
            }
        }
        Ok(_) => w.push(StoreWarning::ImportNotReadBack),
        Err(e) => w.push(StoreWarning::ImportFailed(e)),
    }
}

// Each backend (`imp`) provides the same three functions. `name` is `None` for the device
// key and `Some(secret name)` for a named secret (already checked by `Store::save_secret`).
//
// - `load(dir, name, w)`: reads it; `Ok(None)` if none is stored (or, on Linux, none is
//   reachable: a locked or absent Secret Service with no file). The caller must therefore
//   never treat `None` as licence to replace an existing certificate; see `Store::identity`.
//   Errors are `NetError::Io`: the store cannot be read or (Windows) the blob cannot be
//   unprotected.
// - `save(dir, name, secret, w)`: stores it, replacing any previous one; `NetError::Io` if it
//   cannot be protected or written.
// - `delete(dir, name)`: removes it; a missing one is not an error. `NetError::Io` if the
//   store refuses the removal.
pub(super) use imp::{delete, load, save};

/// macOS: Keychain generic password.
#[cfg(target_os = "macos")]
mod imp {
    use super::{NetError, Path, SECRETS, SERVICE, StoreWarning};
    use security_framework::passwords::{delete_generic_password, get_generic_password, set_generic_password};

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    /// The Keychain service and account: the device key's are the store's directory path,
    /// a named secret's add `#name` under its own service.
    fn target(dir: &Path, name: Option<&str>) -> (&'static str, String) {
        let account = dir.to_string_lossy().into_owned();
        match name {
            None => (SERVICE, account),
            Some(name) => (SECRETS, format!("{account}#{name}")),
        }
    }

    /// Wraps a Security framework error as [`NetError::Io`].
    fn err(e: &security_framework::base::Error) -> NetError {
        NetError::Io(format!("keychain: {e}"))
    }

    /// Backend of [`super::load`]; the Keychain has nothing to warn about.
    pub(crate) fn load(
        dir: &Path,
        name: Option<&str>,
        _w: &mut Vec<StoreWarning>,
    ) -> Result<Option<Vec<u8>>, NetError> {
        let (service, account) = target(dir, name);
        match get_generic_password(service, &account) {
            Ok(k) => Ok(Some(k)),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(err(&e)),
        }
    }

    /// Backend of [`super::save`]; the Keychain has nothing to warn about.
    pub(crate) fn save(
        dir: &Path,
        name: Option<&str>,
        secret: &[u8],
        _w: &mut Vec<StoreWarning>,
    ) -> Result<(), NetError> {
        let (service, account) = target(dir, name);
        set_generic_password(service, &account, secret).map_err(|e| err(&e))
    }

    /// Backend of [`super::delete`].
    pub(crate) fn delete(dir: &Path, name: Option<&str>) -> Result<(), NetError> {
        let (service, account) = target(dir, name);
        match delete_generic_password(service, &account) {
            Err(e) if e.code() != NOT_FOUND => Err(err(&e)),
            _ => Ok(()),
        }
    }
}

/// Windows: DPAPI blob in `key.dpapi`.
#[cfg(windows)]
#[allow(unsafe_code)] // FFI to DPAPI; every call is wrapped and its buffers are owned here.
mod imp {
    use super::{NetError, Path, StoreWarning};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    /// Wraps a message as [`NetError::Io`].
    fn err(m: impl std::fmt::Display) -> NetError {
        NetError::Io(format!("dpapi: {m}"))
    }

    /// Borrows `b` as a DPAPI blob; `b` must outlive the blob.
    fn blob(b: &[u8]) -> Result<CRYPT_INTEGER_BLOB, NetError> {
        Ok(CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(b.len()).map_err(err)?,
            pbData: b.as_ptr().cast_mut(),
        })
    }

    /// Copies a DPAPI-allocated output blob into a `Vec` and frees it.
    ///
    /// # Safety
    ///
    /// `out` must be a blob filled in by a successful DPAPI call, not yet freed.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // An empty or null result has nothing to read (a null slice pointer is undefined even for length 0).
        if out.pbData.is_null() || out.cbData == 0 {
            return Vec::new();
        }
        let len = out.cbData as usize;
        // SAFETY: per the contract, pbData points to cbData readable bytes.
        let v = unsafe { std::slice::from_raw_parts(out.pbData, len) }.to_vec();
        // The buffer may hold a plaintext key: wipe it (volatile, so the compiler keeps the writes).
        for i in 0..len {
            // SAFETY: `i < len` bytes of the DPAPI allocation, which this function owns until freed below.
            unsafe { out.pbData.add(i).write_volatile(0) };
        }
        // SAFETY: DPAPI allocates the output with LocalAlloc; freed exactly once here.
        unsafe { LocalFree(out.pbData.cast()) };
        v
    }

    /// Encrypts `plain` for the current user.
    fn protect(plain: &[u8]) -> Result<Vec<u8>, NetError> {
        let input = blob(plain)?;
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: null_mut(),
        };
        // SAFETY: `input` borrows `plain` for the call; `out` is a valid out-pointer.
        let ok = unsafe {
            CryptProtectData(
                &input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(err(std::io::Error::last_os_error()));
        }
        // SAFETY: the call succeeded, so `out` is a fresh DPAPI allocation.
        Ok(unsafe { take(out) })
    }

    /// Decrypts a blob made by [`protect`]; garbage or another user's blob is an error.
    fn unprotect(blob_bytes: &[u8]) -> Result<Vec<u8>, NetError> {
        let input = blob(blob_bytes)?;
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: null_mut(),
        };
        // SAFETY: `input` borrows `blob_bytes` for the call; `out` is a valid out-pointer.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(err(std::io::Error::last_os_error()));
        }
        // SAFETY: the call succeeded, so `out` is a fresh DPAPI allocation.
        Ok(unsafe { take(out) })
    }

    /// The DPAPI file of the device key or of a named secret.
    fn path(dir: &Path, name: Option<&str>) -> std::path::PathBuf {
        dir.join(name.map_or_else(|| "key.dpapi".to_owned(), |n| format!("secret-{n}.dpapi")))
    }

    /// Backend of [`super::load`]; DPAPI has nothing to warn about.
    pub(crate) fn load(
        dir: &Path,
        name: Option<&str>,
        _w: &mut Vec<StoreWarning>,
    ) -> Result<Option<Vec<u8>>, NetError> {
        match std::fs::read(path(dir, name)) {
            Ok(b) => unprotect(&b).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    /// Backend of [`super::save`]; DPAPI has nothing to warn about.
    pub(crate) fn save(
        dir: &Path,
        name: Option<&str>,
        secret: &[u8],
        _w: &mut Vec<StoreWarning>,
    ) -> Result<(), NetError> {
        super::super::write_private(&path(dir, name), &protect(secret)?)
    }

    /// Backend of [`super::delete`].
    pub(crate) fn delete(dir: &Path, name: Option<&str>) -> Result<(), NetError> {
        match std::fs::remove_file(path(dir, name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(e)),
            _ => Ok(()),
        }
    }
}

/// What [`plan`] tells the Linux backend to do with the key it found.
#[cfg(any(free_unix, test))]
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// The Secret Service holds the key; use it.
    UseSecretService,
    /// A `key.der` exists and the Secret Service is empty: move the file's key in.
    Migrate,
    /// The Secret Service is unusable: the file is the store (or there is no key yet).
    UseFile,
}

/// Chooses the backend from what was found. `ss` is `None` when the Secret Service is
/// unreachable or locked, `Some(has_key)` otherwise; `file` is whether `key.der` exists.
#[cfg(any(free_unix, test))]
fn plan(ss: Option<bool>, file: bool) -> Plan {
    match ss {
        Some(true) => Plan::UseSecretService,
        Some(false) if file => Plan::Migrate,
        _ => Plan::UseFile,
    }
}

/// Linux: Secret Service, with the 0600 `key.der` file as the fallback.
#[cfg(free_unix)]
mod imp {
    use super::{
        NetError, Path, Plan, SECRETS, SERVICE, StoreWarning, file, finish_import, plan, remove_stale_key_file,
    };
    use crate::transport::io;
    use secret_service::EncryptionType;
    use secret_service::blocking::{Collection, SecretService};
    use std::collections::HashMap;

    /// Runs `f` against the default collection on a fresh thread, so the blocking zbus
    /// calls can never run inside the caller's tokio runtime (that would panic). `None`
    /// if the service is unreachable or the collection is locked: unlocking could pop an
    /// interactive prompt, so a locked keyring counts as unavailable. The DH session is
    /// used so the secret is encrypted on the bus.
    fn with_collection<T: Send>(f: impl FnOnce(&Collection<'_>) -> Option<T> + Send) -> Option<T> {
        std::thread::scope(|s| {
            s.spawn(|| {
                let ss = SecretService::connect(EncryptionType::Dh).ok()?;
                let col = ss.get_any_collection().ok()?;
                if col.is_locked().ok()? {
                    return None;
                }
                f(&col)
            })
            .join()
            .ok()
            .flatten()
        })
    }

    /// The item attributes identifying this store's key, or its named secret.
    fn attrs(dir: &Path, name: Option<&str>) -> HashMap<&'static str, String> {
        // A different service, so a search for the device key never finds a named secret.
        let mut a = HashMap::from([
            ("service", name.map_or(SERVICE, |_| SECRETS).to_owned()),
            ("store", dir.to_string_lossy().into_owned()),
        ]);
        if let Some(name) = name {
            a.insert("secret", name.to_owned());
        }
        a
    }

    /// Borrows owned attributes in the form the Secret Service API takes.
    fn borrowed<'a>(a: &'a HashMap<&'static str, String>) -> HashMap<&'static str, &'a str> {
        a.iter().map(|(k, v)| (*k, v.as_str())).collect()
    }

    /// Reads the key: outer `None` = Secret Service unusable, inner `None` = no item.
    fn ss_get(dir: &Path, name: Option<&str>) -> Option<Option<Vec<u8>>> {
        let a = attrs(dir, name);
        with_collection(|c| {
            let items = c.search_items(borrowed(&a)).ok()?;
            match items.first() {
                Some(i) => i.get_secret().ok().map(Some),
                None => Some(None),
            }
        })
    }

    /// Stores the key and reads it back; `true` only if it came back identical.
    fn ss_set(dir: &Path, name: Option<&str>, key: &[u8]) -> bool {
        let a = attrs(dir, name);
        let stored = with_collection(|c| {
            c.create_item(
                "Overdesk device key",
                borrowed(&a),
                key,
                true,
                "application/octet-stream",
            )
            .ok()
            .map(|_| ())
        });
        stored.is_some() && ss_get(dir, name) == Some(Some(key.to_vec()))
    }

    /// Removes every Secret Service item of the device key or of a named secret. `false` if the
    /// service could not be reached or an item could not be removed, so a secret may remain.
    fn ss_delete(dir: &Path, name: Option<&str>) -> bool {
        let a = attrs(dir, name);
        with_collection(|c| {
            for i in c.search_items(borrowed(&a)).ok()? {
                i.delete().ok()?;
            }
            Some(())
        })
        .is_some()
    }

    /// Backend of [`super::load`]. A named secret is the Secret Service's, else the file's;
    /// the device key also migrates a `key.der` into the Secret Service.
    pub(crate) fn load(dir: &Path, name: Option<&str>, w: &mut Vec<StoreWarning>) -> Result<Option<Vec<u8>>, NetError> {
        let ss = ss_get(dir, name);
        if name.is_some() {
            return match ss {
                Some(Some(secret)) => Ok(Some(secret)),
                _ => file::load(dir, name, w),
            };
        }
        let file_key = file::load(dir, None, w)?;
        match plan(ss.as_ref().map(Option::is_some), file_key.is_some()) {
            Plan::UseSecretService => {
                // A key.der identical to the Secret Service key is a leftover of an
                // interrupted migration; a failed delete is retried on the next load.
                if file_key.is_some() && file_key == ss.clone().flatten() {
                    remove_stale_key_file(dir, w);
                }
                Ok(ss.flatten())
            }
            Plan::Migrate => {
                if let Some(key) = &file_key {
                    // `ss_set` already read the key back.
                    let stored = if ss_set(dir, None, key) {
                        Ok(Some(key.clone()))
                    } else {
                        Err("the Secret Service did not take the key".to_owned())
                    };
                    finish_import(dir, key, stored, w);
                }
                Ok(file_key)
            }
            Plan::UseFile => Ok(file_key),
        }
    }

    /// Backend of [`super::save`]: the Secret Service, else the 0600 file.
    pub(crate) fn save(
        dir: &Path,
        name: Option<&str>,
        secret: &[u8],
        w: &mut Vec<StoreWarning>,
    ) -> Result<(), NetError> {
        if ss_set(dir, name, secret) {
            // The file would only hold a stale copy of the secret now.
            if name.is_some() {
                return file::delete(dir, name);
            }
            remove_stale_key_file(dir, w);
            return Ok(());
        }
        file::save(dir, name, secret, w)
    }

    /// Backend of [`super::delete`]: from both places.
    pub(crate) fn delete(dir: &Path, name: Option<&str>) -> Result<(), NetError> {
        let ss_clean = ss_delete(dir, name);
        file::delete(dir, name)?;
        if ss_clean {
            Ok(())
        } else {
            Err(io(
                "the Secret Service could not be reached or cleared; the secret may remain",
            ))
        }
    }
}

/// Platforms with no OS key store: the file is the store.
#[cfg(not(any(target_os = "macos", windows, free_unix)))]
use file as imp;

/// The 0600 `key.der` file.
#[cfg(not(any(target_os = "macos", windows)))]
mod file {
    use super::{NetError, Path, StoreWarning};
    use crate::transport::io;

    /// The 0600 file of the device key or of a named secret.
    fn path(dir: &Path, name: Option<&str>) -> std::path::PathBuf {
        dir.join(name.map_or_else(|| "key.der".to_owned(), |n| format!("secret-{n}.bin")))
    }

    /// Backend of [`super::load`]; a plain file has nothing to warn about.
    pub(crate) fn load(
        dir: &Path,
        name: Option<&str>,
        _w: &mut Vec<StoreWarning>,
    ) -> Result<Option<Vec<u8>>, NetError> {
        match std::fs::read(path(dir, name)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(e)),
        }
    }

    /// Backend of [`super::save`]; a plain file has nothing to warn about.
    pub(crate) fn save(
        dir: &Path,
        name: Option<&str>,
        secret: &[u8],
        _w: &mut Vec<StoreWarning>,
    ) -> Result<(), NetError> {
        super::super::write_private(&path(dir, name), secret)
    }

    /// Backend of [`super::delete`].
    pub(crate) fn delete(dir: &Path, name: Option<&str>) -> Result<(), NetError> {
        match std::fs::remove_file(path(dir, name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(e)),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Plan, plan};
    #[cfg(any(target_os = "macos", windows, free_unix))]
    use super::{StoreWarning, finish_import};

    /// The backend choice for every combination of Secret Service state and file presence.
    #[test]
    fn plan_covers_all_states() {
        assert_eq!(plan(Some(true), true), Plan::UseSecretService);
        assert_eq!(plan(Some(true), false), Plan::UseSecretService);
        assert_eq!(plan(Some(false), true), Plan::Migrate);
        assert_eq!(plan(Some(false), false), Plan::UseFile);
        assert_eq!(plan(None, true), Plan::UseFile);
        assert_eq!(plan(None, false), Plan::UseFile);
    }

    /// The legacy file goes only when the OS store gave back the identical key; every other
    /// outcome keeps it and says why.
    #[cfg(any(target_os = "macos", windows, free_unix))]
    #[test]
    fn an_import_deletes_key_der_only_after_an_identical_read_back() {
        let dir = std::env::temp_dir().join(format!("overdesk-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("key.der");
        std::fs::write(&file, b"key").unwrap();
        let mut w = Vec::new();
        finish_import(&dir, b"key", Ok(None), &mut w);
        finish_import(&dir, b"key", Ok(Some(b"other".to_vec())), &mut w);
        finish_import(&dir, b"key", Err("no".into()), &mut w);
        assert!(file.exists());
        assert_eq!(
            w,
            [
                StoreWarning::ImportNotReadBack,
                StoreWarning::ImportNotReadBack,
                StoreWarning::ImportFailed("no".into())
            ]
        );
        finish_import(&dir, b"key", Ok(Some(b"key".to_vec())), &mut w);
        assert!(!file.exists());
        assert_eq!(w.len(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
