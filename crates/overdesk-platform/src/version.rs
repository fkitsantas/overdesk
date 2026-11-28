// SPDX-License-Identifier: AGPL-3.0-only
//! The operating system's name and version as a person reads it, for logs and bug reports.

/// This computer's OS and version: `macOS 15.2`, `Windows 11 (build 26100)`, the distribution's
/// pretty name on Linux, `FreeBSD 14.1-RELEASE` on the BSDs. Never fails: when the system will
/// not say, it is the bare OS name (`std::env::consts::OS`). May block briefly (a file read or a process).
pub fn os_version() -> String {
    read().unwrap_or_else(|| std::env::consts::OS.to_owned())
}

/// The version the system reports, if it does.
fn read() -> Option<String> {
    #[cfg(target_os = "macos")]
    return mac();
    #[cfg(windows)]
    return windows();
    #[cfg(target_os = "linux")]
    return std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| pretty_name(&text));
    #[cfg(any(target_os = "freebsd", target_os = "openbsd"))]
    return std::process::Command::new("uname")
        .arg("-sr")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty());
    #[cfg(not(any(
        target_os = "macos",
        windows,
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd"
    )))]
    None
}

/// The `PRETTY_NAME` of an `os-release` file, without its quotes.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))] // only Linux reads the file
fn pretty_name(os_release: &str) -> Option<String> {
    os_release
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches(['"', '\'']).to_owned())
        .filter(|v| !v.is_empty())
}

/// `Windows 11 (build N)` from 22000 on, else `Windows 10 (build N)`.
#[cfg_attr(not(any(windows, test)), allow(dead_code))] // only Windows asks the system for a build
fn windows_name(build: u32) -> String {
    format!("Windows {} (build {build})", if build >= 22_000 { 11 } else { 10 })
}

/// `macOS <kern.osproductversion>`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // one `sysctlbyname` call; its invariant is stated at the call
fn mac() -> Option<String> {
    use std::ffi::{c_char, c_int, c_void};
    unsafe extern "C" {
        /// libSystem's named sysctl read.
        fn sysctlbyname(
            name: *const c_char,
            old: *mut c_void,
            old_len: *mut usize,
            new: *mut c_void,
            new_len: usize,
        ) -> c_int;
    }
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: the name is NUL-terminated; `buf` is writable for `len` bytes, which the call
    // updates to the bytes written; no new value is set (null, 0).
    let rc = unsafe {
        sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    let text = std::str::from_utf8(buf.get(..len)?).ok()?.trim_end_matches('\0');
    (rc == 0 && !text.is_empty()).then(|| format!("macOS {text}"))
}

/// `RtlGetVersion`'s build number, as [`windows_name`].
#[cfg(windows)]
#[allow(unsafe_code)] // one `RtlGetVersion` call; its invariant is stated at the call
fn windows() -> Option<String> {
    use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
    // SAFETY: an all-zero OSVERSIONINFOW is a valid value (integers and a wide-char array).
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = size_of::<OSVERSIONINFOW>() as u32;
    // SAFETY: `info` is writable and its size field is set, as the call requires.
    let status = unsafe { RtlGetVersion(&mut info) };
    (status == 0).then(|| windows_name(info.dwBuildNumber))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_11_starts_at_build_22000() {
        assert_eq!(windows_name(26_100), "Windows 11 (build 26100)");
        assert_eq!(windows_name(22_000), "Windows 11 (build 22000)");
        assert_eq!(windows_name(19_045), "Windows 10 (build 19045)");
    }

    #[test]
    fn the_pretty_name_loses_its_quotes_and_missing_means_none() {
        let file = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(pretty_name(file).as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(pretty_name("PRETTY_NAME=Arch Linux\n").as_deref(), Some("Arch Linux"));
        assert_eq!(pretty_name("NAME=x\n"), None);
        assert_eq!(pretty_name("PRETTY_NAME=\"\"\n"), None);
    }

    #[test]
    fn this_computer_has_a_version_string() {
        let v = os_version();
        assert!(!v.is_empty() && !v.contains('\0'), "{v:?}");
        #[cfg(target_os = "macos")]
        assert!(v.starts_with("macOS 1") || v.starts_with("macOS 2"), "{v}");
    }
}
