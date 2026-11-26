// SPDX-License-Identifier: AGPL-3.0-only
//! Where the device's private key (PKCS#8 DER) lives.
//!
//! - macOS: a Keychain generic password, service [`SERVICE`], account = store directory.
//! - Windows: a DPAPI blob (current user) in `key.dpapi` inside the store directory.
//! - Elsewhere: the plain `key.der` file with mode 0600.
//!
//! shortcut: Linux has no OS key store wired in, so the 0600 file stays; use the Secret
//! Service or a TPM when a Linux installer ships.

use std::path::Path;

use crate::transport::NetError;

/// Keychain service name of the device key on macOS.
#[cfg(target_os = "macos")]
const SERVICE: &str = "org.overdesk.Overdesk.device-key";

/// Reads the key; `Ok(None)` if none is stored yet.
///
/// # Errors
///
/// Returns [`NetError::Io`] if the store cannot be read or (Windows) the blob cannot be
/// unprotected.
pub(super) fn load(dir: &Path) -> Result<Option<Vec<u8>>, NetError> {
    imp::load(dir)
}

/// Stores the key, replacing any previous one.
///
/// # Errors
///
/// Returns [`NetError::Io`] if the key cannot be protected or written.
pub(super) fn save(dir: &Path, key: &[u8]) -> Result<(), NetError> {
    imp::save(dir, key)
}

/// Removes the stored key; removing a missing key is not an error. Test cleanup only.
///
/// # Errors
///
/// Returns [`NetError::Io`] if the store refuses the removal.
#[cfg(test)]
pub(super) fn delete(dir: &Path) -> Result<(), NetError> {
    imp::delete(dir)
}

/// macOS: Keychain generic password.
#[cfg(target_os = "macos")]
mod imp {
    use super::{NetError, Path, SERVICE};
    #[cfg(test)]
    use security_framework::passwords::delete_generic_password;
    use security_framework::passwords::{get_generic_password, set_generic_password};

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    /// The Keychain account for a store: its directory path.
    fn account(dir: &Path) -> String {
        dir.to_string_lossy().into_owned()
    }

    /// Wraps a Security framework error as [`NetError::Io`].
    fn err(e: &security_framework::base::Error) -> NetError {
        NetError::Io(format!("keychain: {e}"))
    }

    /// See [`super::load`].
    pub(super) fn load(dir: &Path) -> Result<Option<Vec<u8>>, NetError> {
        match get_generic_password(SERVICE, &account(dir)) {
            Ok(k) => Ok(Some(k)),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(err(&e)),
        }
    }

    /// See [`super::save`].
    pub(super) fn save(dir: &Path, key: &[u8]) -> Result<(), NetError> {
        set_generic_password(SERVICE, &account(dir), key).map_err(|e| err(&e))
    }

    /// See [`super::delete`].
    #[cfg(test)]
    pub(super) fn delete(dir: &Path) -> Result<(), NetError> {
        match delete_generic_password(SERVICE, &account(dir)) {
            Err(e) if e.code() != NOT_FOUND => Err(err(&e)),
            _ => Ok(()),
        }
    }
}

/// Windows: DPAPI blob in `key.dpapi`.
#[cfg(windows)]
#[allow(unsafe_code)] // FFI to DPAPI; every call is wrapped and its buffers are owned here.
mod imp {
    use super::{NetError, Path};
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
        // SAFETY: per the contract, pbData points to cbData readable bytes.
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
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

    /// See [`super::load`].
    pub(super) fn load(dir: &Path) -> Result<Option<Vec<u8>>, NetError> {
        match std::fs::read(dir.join("key.dpapi")) {
            Ok(b) => unprotect(&b).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    /// See [`super::save`].
    pub(super) fn save(dir: &Path, key: &[u8]) -> Result<(), NetError> {
        super::super::write_private(&dir.join("key.dpapi"), &protect(key)?)
    }

    /// See [`super::delete`].
    #[cfg(test)]
    pub(super) fn delete(dir: &Path) -> Result<(), NetError> {
        match std::fs::remove_file(dir.join("key.dpapi")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(e)),
            _ => Ok(()),
        }
    }
}

/// Everything else: the 0600 `key.der` file.
#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::{NetError, Path};

    /// See [`super::load`].
    pub(super) fn load(dir: &Path) -> Result<Option<Vec<u8>>, NetError> {
        match std::fs::read(dir.join("key.der")) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(NetError::Io(e.to_string())),
        }
    }

    /// See [`super::save`].
    pub(super) fn save(dir: &Path, key: &[u8]) -> Result<(), NetError> {
        super::super::write_private(&dir.join("key.der"), key)
    }

    /// See [`super::delete`].
    #[cfg(test)]
    pub(super) fn delete(dir: &Path) -> Result<(), NetError> {
        match std::fs::remove_file(dir.join("key.der")) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(NetError::Io(e.to_string())),
            _ => Ok(()),
        }
    }
}
