// SPDX-License-Identifier: AGPL-3.0-only
//! "Open at login", per user and never admin: a LaunchAgent in `~/Library/LaunchAgents`
//! on macOS, the `HKCU\...\Run` key on Windows. The app takes no start-up arguments, so
//! none are registered.
//!
//! macOS uses the LaunchAgent mode on purpose: it only writes a plist the user owns and
//! works for an ad-hoc-signed app. The AppleScript mode would trigger an Automation
//! permission prompt for System Events, and SMAppService needs a signed, notarised bundle.

use std::path::Path;

use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode, WindowsEnableMode};

/// The login item's name: the app's bundle identifier, which is also the LaunchAgent label.
const NAME: &str = "org.overdesk.Overdesk";
/// Marker file in the store directory: the first-run default was already applied, so a
/// user who turned the setting off is never switched back on.
const MARKER: &str = "login-default-applied";

/// Whether `exe` is an installed copy rather than a development build: inside a macOS
/// `.app` bundle, or under `local_app_data\Programs` on Windows. Deliberately conservative:
/// anything under a `target` directory (a `cargo run` or a freshly packaged build) is not.
pub fn is_packaged(exe: &Path, local_app_data: Option<&Path>) -> bool {
    let lower = exe.to_string_lossy().to_lowercase().replace('\\', "/");
    if lower.split('/').any(|part| part == "target") {
        return false;
    }
    let installed_windows = local_app_data.is_some_and(|dir| {
        let programs = format!("{}/programs/", dir.to_string_lossy().to_lowercase().replace('\\', "/"));
        lower.starts_with(&programs)
    });
    lower.contains(".app/contents/macos/") || installed_windows
}

/// The login item for the running executable.
fn item() -> Result<AutoLaunch, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    AutoLaunchBuilder::new()
        .set_app_name(NAME)
        .set_app_path(&exe.to_string_lossy())
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
        .build()
        .map_err(|e| e.to_string())
}

/// Whether Overdesk is registered to open at login.
pub fn is_enabled() -> bool {
    item()
        .and_then(|i| i.is_enabled().map_err(|e| e.to_string()))
        .unwrap_or(false)
}

/// Registers or removes the login item.
pub fn set(on: bool) -> Result<(), String> {
    let item = item()?;
    if on { item.enable() } else { item.disable() }.map_err(|e| e.to_string())
}

/// On the first run of a packaged build, turns "Open at login" on once, so Overdesk
/// behaves like a background utility. Does nothing for development builds, and never
/// again after the first run, so turning it off sticks.
pub fn apply_default(store: &Path) {
    let marker = store.join(MARKER);
    let local = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from);
    let packaged = std::env::current_exe().is_ok_and(|exe| is_packaged(&exe, local.as_deref()));
    if !packaged || marker.exists() {
        return;
    }
    // Only remember the default once it took; a failure is retried on the next start.
    if set(true).is_ok()
        && std::fs::create_dir_all(store)
            .and_then(|()| std::fs::write(marker, b""))
            .is_err()
    {
        eprintln!("overdesk: could not record the login default");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_only_when_installed() {
        let local = Path::new(r"C:\Users\Ana\AppData\Local");
        assert!(is_packaged(
            Path::new("/Users/a/Applications/Overdesk.app/Contents/MacOS/overdesk"),
            None
        ));
        assert!(is_packaged(
            Path::new(r"C:\Users\ana\appdata\local\Programs\Overdesk\overdesk.exe"),
            Some(local)
        ));
        assert!(!is_packaged(
            Path::new("/Users/a/src/Overdesk/target/debug/overdesk"),
            None
        ));
        assert!(!is_packaged(
            Path::new("/Users/a/src/Overdesk/target/release/Overdesk.app/Contents/MacOS/overdesk"),
            None
        ));
        assert!(!is_packaged(
            Path::new(r"C:\src\target\release\overdesk.exe"),
            Some(local)
        ));
        assert!(!is_packaged(Path::new(r"C:\Tools\overdesk.exe"), Some(local)));
    }
}
