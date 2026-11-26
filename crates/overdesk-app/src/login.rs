// SPDX-License-Identifier: AGPL-3.0-only
//! "Open at login", per user and never admin: a LaunchAgent in `~/Library/LaunchAgents`
//! on macOS, the `HKCU\...\Run` key on Windows, an XDG autostart `.desktop` file in
//! `~/.config/autostart` on Linux. The app takes no start-up arguments, so
//! none are registered.
//!
//! macOS uses the LaunchAgent mode on purpose: it only writes a plist the user owns and
//! works for an ad-hoc-signed app. The AppleScript mode would trigger an Automation
//! permission prompt for System Events, and SMAppService needs a signed, notarised bundle.

use std::path::{Path, PathBuf};

use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode, WindowsEnableMode};

/// The login item's name: the app's bundle identifier, which is also the LaunchAgent label.
const NAME: &str = "org.overdesk.Overdesk";
/// Marker file in the store directory: the first-run default was already applied, so a
/// user who turned the setting off is never switched back on.
const MARKER: &str = "login-default-applied";

/// Whether `exe` is an installed copy rather than a development build: inside a macOS
/// `.app` bundle, under `local_app_data\Programs` on Windows, under `/usr/bin`, `/usr/local/bin`
/// or `/opt` on Linux (a .deb), or an AppImage (`appimage` is the `APPIMAGE` path).
/// Deliberately conservative: anything under a `target` directory (a `cargo run` or a
/// freshly packaged build) is not.
pub fn is_packaged(exe: &Path, local_app_data: Option<&Path>, appimage: Option<&Path>) -> bool {
    let exe = launch_path(exe, appimage);
    let lower = exe.to_string_lossy().to_lowercase().replace('\\', "/");
    if lower.split('/').any(|part| part == "target") {
        return false;
    }
    let installed_windows = local_app_data.is_some_and(|dir| {
        let programs = format!("{}/programs/", dir.to_string_lossy().to_lowercase().replace('\\', "/"));
        lower.starts_with(&programs)
    });
    let installed_linux = appimage.is_some_and(Path::is_absolute)
        || ["/usr/bin/", "/usr/local/bin/", "/opt/"]
            .iter()
            .any(|d| lower.starts_with(d));
    lower.contains(".app/contents/macos/") || installed_windows || installed_linux
}

/// What to register for login: the AppImage file itself when running from one (the
/// executable is then inside a temporary mount that vanishes on exit), else `exe`.
fn launch_path<'a>(exe: &'a Path, appimage: Option<&'a Path>) -> &'a Path {
    appimage.filter(|a| a.is_absolute()).unwrap_or(exe)
}

/// The login item for the running executable.
fn item() -> Result<AutoLaunch, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    let path = launch_path(&exe, appimage.as_deref());
    AutoLaunchBuilder::new()
        .set_app_name(NAME)
        .set_app_path(&path.to_string_lossy())
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
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    let packaged = std::env::current_exe().is_ok_and(|exe| is_packaged(&exe, local.as_deref(), appimage.as_deref()));
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
        let mac = Path::new("/Users/a/Applications/Overdesk.app/Contents/MacOS/overdesk");
        assert!(is_packaged(mac, None, None));
        assert!(is_packaged(
            Path::new(r"C:\Users\ana\appdata\local\Programs\Overdesk\overdesk.exe"),
            Some(local),
            None
        ));
        assert!(!is_packaged(
            Path::new("/Users/a/src/Overdesk/target/debug/overdesk"),
            None,
            None
        ));
        assert!(!is_packaged(
            Path::new("/Users/a/src/Overdesk/target/release/Overdesk.app/Contents/MacOS/overdesk"),
            None,
            None
        ));
        assert!(!is_packaged(
            Path::new(r"C:\src\target\release\overdesk.exe"),
            Some(local),
            None
        ));
        assert!(!is_packaged(Path::new(r"C:\Tools\overdesk.exe"), Some(local), None));
    }

    #[test]
    fn linux_installs_and_appimage() {
        let mount = Path::new("/tmp/.mount_OverdeXYZ/usr/bin/overdesk");
        let image = Path::new("/home/a/Apps/Overdesk.AppImage");
        assert!(is_packaged(Path::new("/usr/bin/overdesk"), None, None));
        assert!(is_packaged(Path::new("/opt/Overdesk/overdesk"), None, None));
        assert!(is_packaged(mount, None, Some(image)));
        assert!(!is_packaged(mount, None, None)); // the mount alone proves nothing
        assert!(!is_packaged(mount, None, Some(Path::new("relative.AppImage"))));
        assert!(!is_packaged(
            mount,
            None,
            Some(Path::new("/home/a/src/target/release/Overdesk.AppImage"))
        ));
        assert!(!is_packaged(
            Path::new("/home/a/src/Overdesk/target/debug/overdesk"),
            None,
            None
        ));
        assert_eq!(launch_path(mount, Some(image)), image);
        assert_eq!(launch_path(mount, None), mount);
    }
}
