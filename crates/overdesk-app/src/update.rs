// SPDX-License-Identifier: AGPL-3.0-only
//! Signed self-update. Overdesk looks for a newer version at startup and every six
//! hours, downloads it, and checks its Ed25519 (minisign) signature against the public key
//! compiled into the app. A verified update is held in memory ("staged"); the idle screen
//! then offers a quiet "Restart" button, so an update never replaces the app while someone
//! is using it. Anything that goes wrong is dropped silently: the next check simply retries.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use cargo_packager_updater::semver::Version;
use cargo_packager_updater::{Config, Update, UpdaterBuilder};
use overdeskd::controller::Phase;

use overdesk_core::engine::Mode;

/// The release public key (`updater.pub`). The matching private key stays on the release
/// machine, outside the repository (see `docs/RELEASING.md`).
const PUBKEY: &str = include_str!("../updater.pub");
/// Where updates are published. `{{target}}`, `{{arch}}` and `{{current_version}}` are filled
/// in by the updater crate. Tests override it with `OVERDESK_UPDATE_URL`.
const ENDPOINT: &str = "https://overdesk.org/updates/{{target}}/{{arch}}/{{current_version}}";
/// How often the background check repeats.
pub const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// The running version. `OVERDESK_VERSION` at build time overrides it, so release tests can
/// build a "newer" or "older" app without editing `Cargo.toml` (see `scripts/package.sh`).
const VERSION: &str = match option_env!("OVERDESK_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// A downloaded update whose signature has been verified, waiting for a restart.
pub struct Staged {
    /// The update's metadata and installer.
    update: Update,
    /// The verified download.
    bytes: Vec<u8>,
}

impl std::fmt::Debug for Staged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Staged({})", self.update.version)
    }
}

impl Staged {
    /// The version this update installs.
    pub fn version(&self) -> &str {
        &self.update.version
    }

    /// Replaces the app with the staged version. The caller restarts afterwards.
    pub fn install(&self) -> Result<(), String> {
        self.update.install(self.bytes.clone()).map_err(|e| e.to_string())
    }
}

/// Whether an update may be installed now: never while the user is driving another device
/// (or being driven), and not mid-connect or mid-pairing either.
pub fn install_allowed(phase: &Phase) -> bool {
    match phase {
        Phase::Idle | Phase::Lost { .. } | Phase::Error(_) => true,
        Phase::Connected { mode, .. } => matches!(mode, Mode::Local),
        Phase::Connecting { .. } | Phase::Pairing { .. } => false,
    }
}

/// Reads a flag the way policy sources write it: `1`, `true` or `yes` (any case) means on.
fn is_on(text: &str) -> bool {
    matches!(text.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

/// Whether IT has turned updates off: the `OVERDESK_DISABLE_UPDATES` variable, or the
/// managed policy (macOS `DisableUpdates` in `/Library/Managed Preferences/org.overdesk.Overdesk.plist`;
/// Windows `HKLM\Software\Policies\Overdesk\DisableUpdates` = 1).
fn disabled() -> bool {
    std::env::var("OVERDESK_DISABLE_UPDATES").is_ok_and(|v| is_on(&v)) || policy_disabled()
}

/// The macOS policy file. `plutil` reads both XML and binary plists; it prints `true` for a
/// boolean `DisableUpdates` and fails if the file or key is missing.
#[cfg(target_os = "macos")]
fn policy_disabled() -> bool {
    std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "DisableUpdates", "raw", "-o", "-"])
        .arg("/Library/Managed Preferences/org.overdesk.Overdesk.plist")
        .output()
        .is_ok_and(|o| o.status.success() && is_on(&String::from_utf8_lossy(&o.stdout)))
}

/// The Windows policy key: a DWORD `DisableUpdates` = 1 under `HKLM\Software\Policies\Overdesk`.
#[cfg(windows)]
#[allow(unsafe_code)] // one registry read; the buffers are local and sized by the API contract
fn policy_disabled() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (key, name) = (wide(r"Software\Policies\Overdesk"), wide("DisableUpdates"));
    let (mut value, mut size) = (0u32, 4u32);
    // SAFETY: `key` and `name` are NUL-terminated; `value`/`size` are valid for the call.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut value).cast(),
            &mut size,
        )
    };
    status == 0 && value == 1
}

/// No managed policy file exists on other platforms.
#[cfg(not(any(target_os = "macos", windows)))]
fn policy_disabled() -> bool {
    false
}

/// Whether the installed app can be replaced without an admin prompt. The updater crate
/// would otherwise ask for administrator rights on macOS; Overdesk never does, so it skips
/// the update instead. Windows installs per user and always qualifies.
fn replaceable_without_admin() -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(exe) = std::env::current_exe() else { return false };
        let Some(app) = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")) else {
            return false; // not running from a bundle (a dev build): nothing to replace
        };
        let probe = app.with_file_name(".overdesk-update-probe");
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    }
    #[cfg(not(target_os = "macos"))]
    true
}

/// Checks for an update in the background and, if there is one, downloads and verifies it.
/// `None` means nothing to install: no update, updates disabled, or any failure (including a
/// signature that does not verify, which discards the download).
pub async fn check() -> Option<Arc<Staged>> {
    tokio::task::spawn_blocking(check_blocking)
        .await
        .ok()
        .flatten()
        .map(Arc::new)
}

/// The blocking body of [`check`]; the updater crate uses a blocking HTTP client.
fn check_blocking() -> Option<Staged> {
    if disabled() || !replaceable_without_admin() {
        return None;
    }
    let url = std::env::var("OVERDESK_UPDATE_URL").unwrap_or_else(|_| ENDPOINT.into());
    let config = Config {
        endpoints: vec![url.parse().ok()?],
        pubkey: PUBKEY.trim().into(),
        ..Default::default()
    };
    let current: Version = VERSION.parse().ok()?;
    let checked = UpdaterBuilder::new(current.clone(), config)
        // Refuse anything that is not strictly newer than the running version.
        .version_comparator(|current, release| release.version > current)
        .build()
        .and_then(|updater| updater.check());
    let update = match checked {
        Ok(Some(update)) => update,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("overdesk: update check failed: {e}");
            return None;
        }
    };
    // `download` verifies the signature before returning the bytes.
    match update.download() {
        Ok(bytes) => {
            // The manifest is unsigned: bind its version to the signed file name.
            if let Err(why) = vet(&update.signature, &update.version, &current) {
                eprintln!("overdesk: update discarded: {why}");
                return None;
            }
            eprintln!("overdesk: update {} downloaded and verified", update.version);
            Some(Staged { update, bytes })
        }
        Err(e) => {
            eprintln!("overdesk: update discarded: {e}");
            None
        }
    }
}

/// The version embedded in the signed file name of a minisign signature (base64 of the
/// `.sig` file). The trusted comment `timestamp:<n>\tfile:<name>` is covered by the global
/// signature, and `<name>` is `<product>_<version>_<rest>`. `None` if any of that is missing.
fn signed_version(signature_b64: &str) -> Option<Version> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    let trusted = text.lines().find_map(|l| l.strip_prefix("trusted comment:"))?;
    let name = trusted.split('\t').find_map(|f| f.trim().strip_prefix("file:"))?;
    name.split('_').nth(1)?.parse().ok()
}

/// Decides whether a verified download may be staged: the signed version must exist, equal
/// the manifest's `offered` version and be strictly newer than `current`. The error says why not.
fn vet(signature_b64: &str, offered: &str, current: &Version) -> Result<(), String> {
    let signed = signed_version(signature_b64).ok_or("signature carries no readable version")?;
    if signed.to_string() != offered {
        return Err(format!("manifest offers {offered} but the signed artifact is {signed}"));
    }
    if signed <= *current {
        return Err(format!("{signed} is not newer than {current}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use overdesk_core::engine::Mode;
    use overdeskd::controller::Phase;

    use base64::Engine;
    use cargo_packager_updater::semver::Version;

    use super::{install_allowed, is_on, signed_version, vet};

    /// A minisign signature file (as cargo-packager writes it) with the given trusted comment,
    /// base64-encoded the way `latest.json` carries it.
    fn sig(trusted: &str) -> String {
        let file =
            format!("untrusted comment: signature from cargo-packager secret key\nRUTZe/AAAA\n{trusted}\nmS0UF5AAAA\n");
        base64::engine::general_purpose::STANDARD.encode(file)
    }

    /// The trusted comment cargo-packager writes for an artifact file name.
    fn comment(file: &str) -> String {
        format!("trusted comment: timestamp:1795700885\tfile:{file}")
    }

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn signed_version_reads_the_file_name() {
        let s = sig(&comment("Overdesk_0.1.1_aarch64.app.tar.gz"));
        assert_eq!(signed_version(&s), Some(v("0.1.1")));
        let s = sig(&comment("Overdesk_2.10.3-beta.1_x64-setup.exe"));
        assert_eq!(signed_version(&s), Some(v("2.10.3-beta.1")));
    }

    #[test]
    fn garbage_or_missing_comments_are_rejected() {
        assert_eq!(signed_version(""), None);
        assert_eq!(signed_version("not base64 !!"), None);
        assert_eq!(signed_version(&sig("")), None);
        assert_eq!(signed_version(&sig("trusted comment: timestamp:1")), None);
        assert_eq!(signed_version(&sig(&comment("Overdesk.app.tar.gz"))), None);
        assert_eq!(signed_version(&sig(&comment("Overdesk_latest_aarch64.tar.gz"))), None);
        // an unsigned (untrusted) comment must not count
        let forged = sig("untrusted comment: timestamp:1\tfile:Overdesk_9.9.9_x.tar.gz");
        assert_eq!(signed_version(&forged), None);
    }

    #[test]
    fn only_a_signed_strictly_newer_matching_version_passes() {
        let signed = |f: &str| sig(&comment(f));
        let cur = v("0.1.0");
        assert!(vet(&signed("Overdesk_0.1.1_a.tar.gz"), "0.1.1", &cur).is_ok());
        // manifest claims newer than what was signed (the downgrade attack)
        assert!(vet(&signed("Overdesk_0.1.1_a.tar.gz"), "0.1.2", &cur).is_err());
        // equal and older
        assert!(vet(&signed("Overdesk_0.1.0_a.tar.gz"), "0.1.0", &cur).is_err());
        assert!(vet(&signed("Overdesk_0.0.9_a.tar.gz"), "0.0.9", &cur).is_err());
        // unparseable
        assert!(vet(&signed("Overdesk.app.tar.gz"), "0.1.1", &cur).is_err());
        assert!(vet("junk", "0.1.1", &cur).is_err());
    }

    #[test]
    fn policy_flags_parse() {
        for on in ["1", "true", "True\n", " YES "] {
            assert!(is_on(on), "{on:?}");
        }
        for off in ["0", "false", "", "2", "no"] {
            assert!(!is_on(off), "{off:?}");
        }
    }

    #[test]
    fn never_installs_while_driving_or_connecting() {
        let peer = || "Mac".to_string();
        let connected = |mode| Phase::Connected {
            peer: peer(),
            peer_os: overdesk_proto::Os::MacOs,
            mode,
        };
        assert!(install_allowed(&Phase::Idle));
        assert!(install_allowed(&Phase::Error("x".into())));
        assert!(install_allowed(&connected(Mode::Local)));
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(!install_allowed(&Phase::Connecting { address: addr }));
        assert!(!install_allowed(&Phase::Pairing { peer: peer(), code: 1 }));
    }

    #[test]
    fn bundled_public_key_is_a_minisign_key() {
        // base64 of "untrusted comment: minisign public key"
        assert!(super::PUBKEY.starts_with("dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6"));
    }
}
