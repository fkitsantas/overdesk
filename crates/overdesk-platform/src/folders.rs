// SPDX-License-Identifier: AGPL-3.0-only
//! Where the OS keeps the user's own folders.

use std::path::PathBuf;

/// The user's Downloads folder as Windows knows it (it can be moved or renamed, so
/// `%USERPROFILE%\Downloads` is only a guess). `None` where there is no such answer: on other
/// systems the caller asks the environment, and on Windows when the call fails.
pub fn downloads_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return known_downloads();
    #[cfg(not(windows))]
    None
}

/// `SHGetKnownFolderPath(FOLDERID_Downloads)`, the result freed.
#[cfg(windows)]
#[allow(unsafe_code)] // one shell call and one free; the invariants are stated at each
fn known_downloads() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath};
    let mut path: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: the id outlives the call, a null token means the current user, and `path` is a
    // writable out-pointer that the call sets to a NUL-terminated string it allocated (or leaves null).
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, 0, std::ptr::null_mut(), &mut path) };
    let found = (hr >= 0 && !path.is_null()).then(|| {
        let mut len = 0;
        // SAFETY: the string is NUL-terminated, so reading up to the NUL stays inside it.
        while unsafe { *path.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` units were just read from this string, which stays allocated until freed below.
        PathBuf::from(std::ffi::OsString::from_wide(unsafe {
            std::slice::from_raw_parts(path, len)
        }))
    });
    // SAFETY: the shell allocated `path` for the caller to free, even when the call failed; null is allowed.
    unsafe { CoTaskMemFree(path.cast_const().cast()) };
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_an_absolute_path_and_only_windows_gives_one() {
        // Windows without the folder, or any other system, answers `None`.
        if let Some(p) = downloads_dir() {
            assert!(cfg!(windows) && p.is_absolute(), "{p:?}");
        }
    }
}
