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
        // No backup means no rollback, so refuse to replace the app without one.
        rollback::prepare(&self.update.version)?;
        let installed = self.update.install(self.bytes.clone()).map_err(|e| e.to_string());
        if installed.is_err() {
            rollback::forget(); // the updater put the old app back; nothing is pending
        }
        installed
    }
}

/// Runs first thing at startup: counts a start of a freshly installed version, or rolls a
/// version that keeps failing back to the previous one (and then does not return).
pub fn startup() {
    rollback::startup();
}

/// Waits a few seconds, then declares this start healthy: the update is kept and its backup
/// deleted. Started with the app, so the window being up counts whichever screen it shows
/// (the permissions screen never starts the controller); a crash before this point counts
/// as a failed start.
pub async fn confirm_healthy() {
    tokio::time::sleep(rollback::HEALTHY_AFTER).await;
    rollback::confirm();
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
/// the update instead. Windows installs per user and always qualifies. Linux needs a writable AppImage.
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
    // A .deb belongs to the package manager; only an AppImage replaces itself in place.
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("APPIMAGE").is_some_and(|p| std::fs::OpenOptions::new().append(true).open(p).is_ok())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
    let bad = rollback::bad_versions();
    let checked = UpdaterBuilder::new(current.clone(), config)
        // Refuse anything that is not strictly newer than the running version, and any
        // version that was rolled back after failing to start.
        .version_comparator(move |current, release| release.version > current && !bad.contains(&release.version))
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

/// Automatic rollback of a failed update (macOS). The updater crate keeps the old app only in
/// a temporary directory it deletes, so before installing we copy the bundle to the data
/// directory and write a "pending update" marker. The new version must stay up for
/// [`HEALTHY_AFTER`] to confirm itself; if it is started [`MAX_FAILED_STARTS`] times without
/// getting there, the next start restores the copy and remembers the version as bad.
///
/// shortcut: Windows has no rollback. The NSIS installer runs as a separate process that
/// replaces `%LOCALAPPDATA%\Programs\Overdesk` while this exe is locked, so a backup would
/// have to be restored by a helper that outlives the app. Upgrade path: copy that folder
/// before install and restore it from a tiny per-user `HKCU\...\RunOnce` helper.
///
/// shortcut: Linux has no rollback either. An AppImage could be copied aside like the macOS
/// bundle, but the updater swaps the file while it runs and the restore needs the same
/// start-up hook, which is untested without a Linux desktop. Upgrade: copy `$APPIMAGE` in
/// `prepare` and restore it in `startup`, as on macOS.
mod rollback {
    #![cfg_attr(not(target_os = "macos"), allow(dead_code))]

    use std::path::PathBuf;
    use std::time::Duration;

    use cargo_packager_updater::semver::Version;

    use super::VERSION;

    /// How long the new version must keep running before it counts as healthy.
    pub const HEALTHY_AFTER: Duration = Duration::from_secs(10);
    /// Starts of the new version that did not become healthy before the next one rolls back.
    const MAX_FAILED_STARTS: u32 = 2;

    /// A recorded, not yet confirmed update.
    #[derive(Debug, PartialEq, Eq)]
    struct Marker {
        /// The version that was running before the update.
        old: Version,
        /// The version that was installed.
        new: Version,
        /// The copy of the old bundle.
        backup: PathBuf,
        /// How many starts of `new` began without it becoming healthy.
        attempts: u32,
    }

    /// What a start should do about a pending update.
    #[derive(Debug, PartialEq, Eq)]
    enum Verdict {
        /// The update never took effect, or something else is running: drop the marker.
        Clear,
        /// The new version is starting: record the attempt.
        CountAttempt,
        /// The new version failed repeatedly: restore the backup.
        RollBack,
    }

    impl Marker {
        /// Text form: one `key=value` per line.
        fn render(&self) -> String {
            format!(
                "old={}\nnew={}\nbackup={}\nattempts={}\n",
                self.old,
                self.new,
                self.backup.display(),
                self.attempts
            )
        }

        /// Reads the text form; `None` if any field is missing or malformed.
        fn parse(text: &str) -> Option<Self> {
            let field = |key: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
            };
            Some(Self {
                old: field("old")?.parse().ok()?,
                new: field("new")?.parse().ok()?,
                backup: field("backup")?.into(),
                attempts: field("attempts")?.parse().ok()?,
            })
        }
    }

    /// Decides what a start does. Only the exact version that was installed is ever rolled
    /// back: if anything else runs (the old version after a failed install, a newer one
    /// installed by hand), the marker is stale and restoring would destroy a good install.
    fn decide(marker: &Marker, running: &Version) -> Verdict {
        if *running != marker.new {
            Verdict::Clear
        } else if marker.attempts >= MAX_FAILED_STARTS {
            Verdict::RollBack
        } else {
            Verdict::CountAttempt
        }
    }

    /// The marker file.
    fn marker_path() -> PathBuf {
        crate::store_dir().join("pending-update")
    }

    /// Where the previous bundle is kept.
    fn backup_dir() -> PathBuf {
        crate::store_dir().join("previous")
    }

    /// The file listing versions that were rolled back, one per line.
    fn bad_path() -> PathBuf {
        crate::store_dir().join("bad-updates")
    }

    /// Previews render a fixed state and must never touch the install or the data directory.
    const fn disabled() -> bool {
        cfg!(feature = "preview")
    }

    /// Versions the updater must not offer again.
    pub fn bad_versions() -> Vec<Version> {
        std::fs::read_to_string(bad_path())
            .map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    /// Remembers `version` as bad. Best effort: failing to write only means one more offer.
    fn mark_bad(version: &Version) {
        let mut all = bad_versions();
        if !all.contains(version) {
            all.push(version.clone());
        }
        let text: String = all.iter().map(|v| format!("{v}\n")).collect();
        let _ = std::fs::write(bad_path(), text);
    }

    /// Deletes the marker and the backup.
    pub fn forget() {
        let _ = std::fs::remove_file(marker_path());
        let _ = std::fs::remove_dir_all(backup_dir());
    }

    /// The loaded marker, if a valid one exists.
    fn load() -> Option<Marker> {
        Marker::parse(&std::fs::read_to_string(marker_path()).ok()?)
    }

    /// The running `.app` bundle; `None` when not running from one (a dev build).
    #[cfg(target_os = "macos")]
    fn bundle() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        exe.ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .map(PathBuf::from)
    }

    /// Copies the current bundle aside and records the pending update, before the install.
    #[cfg(target_os = "macos")]
    pub fn prepare(new_version: &str) -> Result<(), String> {
        if disabled() {
            return Ok(());
        }
        let io = |e: std::io::Error| e.to_string();
        let app = bundle().ok_or("not running from an app bundle")?;
        forget();
        std::fs::create_dir_all(backup_dir()).map_err(io)?;
        let copy = backup_dir().join("Overdesk.app");
        // `ditto` keeps symlinks, permissions and the code signature intact.
        let status = std::process::Command::new("/usr/bin/ditto")
            .arg(&app)
            .arg(&copy)
            .status()
            .map_err(io)?;
        if !status.success() {
            forget();
            return Err("could not back up the current version".into());
        }
        let marker = Marker {
            old: VERSION.parse().map_err(|_| "unreadable running version")?,
            new: new_version.parse().map_err(|_| "unreadable update version")?,
            backup: copy,
            attempts: 0,
        };
        std::fs::write(marker_path(), marker.render()).map_err(io)
    }

    /// No backup is made off macOS (see the module note), so installing is always allowed.
    #[cfg(not(target_os = "macos"))]
    pub fn prepare(_new_version: &str) -> Result<(), String> {
        Ok(())
    }

    /// Start-of-process hook: count the attempt, clear a stale marker or roll back.
    pub fn startup() {
        if disabled() {
            return;
        }
        let (Some(mut marker), Ok(running)) = (load(), VERSION.parse::<Version>()) else {
            return;
        };
        match decide(&marker, &running) {
            Verdict::Clear => forget(),
            Verdict::CountAttempt => {
                marker.attempts += 1;
                let _ = std::fs::write(marker_path(), marker.render());
            }
            Verdict::RollBack => {
                // Mark first and drop the marker before touching the install, so a failure
                // below can never repeat on the next start.
                mark_bad(&marker.new);
                let backup = marker.backup.clone();
                let _ = std::fs::remove_file(marker_path());
                match restore(&backup) {
                    Ok(()) => {
                        eprintln!(
                            "overdesk: {} failed to start; rolled back to {}",
                            marker.new, marker.old
                        );
                        let _ = std::fs::remove_dir_all(backup_dir());
                        crate::relaunch(); // exits
                    }
                    Err(e) => eprintln!("overdesk: rollback failed: {e}"),
                }
            }
        }
    }

    /// Replaces the running bundle with `backup`, keeping the failed one until the swap is done.
    #[cfg(target_os = "macos")]
    fn restore(backup: &std::path::Path) -> Result<(), String> {
        let io = |e: std::io::Error| e.to_string();
        let app = bundle().ok_or("not running from an app bundle")?;
        if !backup.is_dir() {
            return Err("the backup is missing".into());
        }
        let staged = app.with_extension("app.restoring");
        let failed = app.with_extension("app.failed");
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&failed);
        let status = std::process::Command::new("/usr/bin/ditto")
            .arg(backup)
            .arg(&staged)
            .status()
            .map_err(io)?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&staged);
            return Err("could not copy the backup".into());
        }
        std::fs::rename(&app, &failed).map_err(io)?;
        if let Err(e) = std::fs::rename(&staged, &app) {
            let _ = std::fs::rename(&failed, &app); // put the failed one back rather than nothing
            return Err(e.to_string());
        }
        let _ = std::fs::remove_dir_all(&failed);
        Ok(())
    }

    /// Nothing is ever recorded off macOS, so there is nothing to restore.
    #[cfg(not(target_os = "macos"))]
    fn restore(_backup: &std::path::Path) -> Result<(), String> {
        Err("rollback is not supported on this platform".into())
    }

    /// The new version has stayed up long enough: keep it and delete the backup.
    pub fn confirm() {
        if disabled() {
            return;
        }
        if let (Some(marker), Ok(running)) = (load(), VERSION.parse::<Version>())
            && marker.new == running
        {
            forget();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{Marker, Verdict, decide};

        /// A marker for 0.1.0 -> 0.1.1 with the given attempt count.
        fn marker(attempts: u32) -> Marker {
            Marker {
                old: "0.1.0".parse().unwrap(),
                new: "0.1.1".parse().unwrap(),
                backup: "/tmp/previous/Overdesk.app".into(),
                attempts,
            }
        }

        #[test]
        fn marker_round_trips_and_rejects_damage() {
            let m = marker(1);
            assert_eq!(Marker::parse(&m.render()), Some(m));
            assert_eq!(Marker::parse(""), None);
            assert_eq!(Marker::parse("old=0.1.0\nnew=x\nbackup=/b\nattempts=0\n"), None);
            assert_eq!(Marker::parse("old=0.1.0\nnew=0.1.1\nbackup=/b\n"), None);
        }

        #[test]
        fn counts_two_starts_then_rolls_back() {
            let new = "0.1.1".parse().unwrap();
            assert_eq!(decide(&marker(0), &new), Verdict::CountAttempt);
            assert_eq!(decide(&marker(1), &new), Verdict::CountAttempt);
            assert_eq!(decide(&marker(2), &new), Verdict::RollBack);
            assert_eq!(decide(&marker(9), &new), Verdict::RollBack);
        }

        #[test]
        fn never_restores_over_any_other_version() {
            for other in ["0.1.0", "0.1.2", "0.2.0", "0.0.1"] {
                assert_eq!(decide(&marker(5), &other.parse().unwrap()), Verdict::Clear, "{other}");
            }
        }
    }
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
