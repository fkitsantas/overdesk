// SPDX-License-Identifier: AGPL-3.0-only
//! What the daemon learns from the environment: where it keeps its state and which OS it is.

use std::path::PathBuf;

use overdesk_net::store::Store;
use overdesk_net::transport::{Identity, NetError};
use overdesk_proto::{Os, log};

/// Where the identity and pinned peers live: `OVERDESK_HOME`, or the per-OS default. The one
/// definition, shared by the app and the command-line daemon so both find the same pairings.
pub fn store_dir() -> PathBuf {
    store_dir_from(
        std::env::var_os("OVERDESK_HOME"),
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("APPDATA"))
            .or_else(|| std::env::var_os("USERPROFILE")),
    )
}

/// [`store_dir`] with the environment passed in. Without a home folder it uses the temp
/// folder, never the working directory, so pairings are not scattered around the disk.
fn store_dir_from(overridden: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(dir) = overridden {
        return dir.into();
    }
    let home = home.map_or_else(std::env::temp_dir, PathBuf::from);
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Overdesk")
    } else if cfg!(windows) {
        home.join("Overdesk")
    } else {
        home.join(".overdesk")
    }
}

/// [`Store::identity`], then logs what the key store worked around on the way. The one place
/// the daemon, the apps and the tools load their identity, so a legacy-key problem is never
/// silent. Blocking, like `identity`.
///
/// # Errors
///
/// As [`Store::identity`].
pub fn load_identity(store: &Store) -> Result<Identity, NetError> {
    let identity = store.identity();
    store.take_warnings().into_iter().for_each(log::warn);
    identity
}

/// The key chord that always takes control back on `os`.
pub const fn escape_chord(os: Os) -> &'static str {
    match os {
        Os::MacOs => "Ctrl+Option+Cmd+Esc",
        Os::Windows | Os::Linux | Os::Android => "Ctrl+Alt+Win+Esc",
    }
}

/// The operating system this build runs on, as announced to peers. The BSDs announce
/// `Linux`: the wire enum is closed (a peer that does not know a new variant cannot decode the
/// hello at all), and the BSD desktops share Linux's PC keyboard conventions.
pub fn this_os() -> Os {
    os_of(std::env::consts::OS)
}

/// [`this_os`] for a named `std::env::consts::OS`.
fn os_of(name: &str) -> Os {
    match name {
        "windows" => Os::Windows,
        "macos" => Os::MacOs,
        _ => Os::Linux,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bsds_announce_linux_on_the_wire() {
        for (name, os) in [
            ("macos", Os::MacOs),
            ("windows", Os::Windows),
            ("linux", Os::Linux),
            ("freebsd", Os::Linux),
            ("openbsd", Os::Linux),
        ] {
            assert_eq!(os_of(name), os, "{name}");
        }
    }

    #[test]
    fn store_dir_prefers_the_override_and_never_the_working_directory() {
        let dir = |o: Option<&str>, h: Option<&str>| store_dir_from(o.map(Into::into), h.map(Into::into));
        assert_eq!(dir(Some("/x"), Some("/home/a")), PathBuf::from("/x"));
        assert!(dir(None, Some("/home/a")).starts_with("/home/a"));
        let fallback = dir(None, None);
        assert!(fallback.starts_with(std::env::temp_dir()), "{fallback:?}");
    }
}
