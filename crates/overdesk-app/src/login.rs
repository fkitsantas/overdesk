// SPDX-License-Identifier: AGPL-3.0-only
//! "Open at login", per user and never admin: a LaunchAgent in `~/Library/LaunchAgents`
//! on macOS, the `HKCU\...\Run` key on Windows, an XDG autostart `.desktop` file in
//! `~/.config/autostart` on Linux (the `auto-launch` crate has no BSD backend, so FreeBSD and
//! OpenBSD write that same file here). The login item passes `--background`, so the app
//! starts hidden in the tray instead of opening its window at every login.
//!
//! macOS uses the LaunchAgent mode on purpose: it only writes a plist the user owns and
//! works for an ad-hoc-signed app. The AppleScript mode would trigger an Automation
//! permission prompt for System Events, and SMAppService needs a signed, notarised bundle.

use std::path::{Path, PathBuf};

#[cfg(not(all(free_unix, not(target_os = "linux"))))]
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
    // has_root, not is_absolute: the same `/...` AppImage path counts on every OS (Windows calls it relative without a drive).
    let installed_linux = appimage.is_some_and(Path::has_root)
        || ["/usr/bin/", "/usr/local/bin/", "/opt/"]
            .iter()
            .any(|d| lower.starts_with(d));
    lower.contains(".app/contents/macos/") || installed_windows || installed_linux
}

/// What to register for login: the AppImage file itself when running from one (the
/// executable is then inside a temporary mount that vanishes on exit), else `exe`.
fn launch_path<'a>(exe: &'a Path, appimage: Option<&'a Path>) -> &'a Path {
    appimage.filter(|a| a.has_root()).unwrap_or(exe)
}

/// The XDG autostart file's text. `Exec` quotes the path, so spaces and `%` survive.
#[cfg(any(test, all(free_unix, not(target_os = "linux"))))]
fn desktop_entry(path: &Path) -> String {
    let exec = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!(
        "[Desktop Entry]\nType=Application\nName=Overdesk\nExec=\"{exec}\" {}\nTerminal=false\n",
        crate::BACKGROUND_ARG
    )
}

/// The BSDs' autostart file, `$XDG_CONFIG_HOME/autostart/<NAME>.desktop` (else under `~/.config`).
#[cfg(all(free_unix, not(target_os = "linux")))]
fn autostart_file() -> Result<PathBuf, String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("no home folder")?;
    Ok(config.join("autostart").join(format!("{NAME}.desktop")))
}

/// The login item for the running executable.
#[cfg(not(all(free_unix, not(target_os = "linux"))))]
fn item() -> Result<AutoLaunch, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
    let path = launch_path(&exe, appimage.as_deref());
    AutoLaunchBuilder::new()
        .set_app_name(NAME)
        .set_app_path(&path.to_string_lossy())
        .set_args(&[crate::BACKGROUND_ARG])
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
        .build()
        .map_err(|e| e.to_string())
}

/// Whether Overdesk is registered to open at login.
#[cfg(all(free_unix, not(target_os = "linux")))]
pub fn is_enabled() -> bool {
    autostart_file().is_ok_and(|f| f.exists())
}

/// Whether Overdesk is registered to open at login.
#[cfg(not(all(free_unix, not(target_os = "linux"))))]
pub fn is_enabled() -> bool {
    item()
        .and_then(|i| i.is_enabled().map_err(|e| e.to_string()))
        .unwrap_or(false)
}

/// Registers or removes the login item.
#[cfg(all(free_unix, not(target_os = "linux")))]
pub fn set(on: bool) -> Result<(), String> {
    let file = autostart_file()?;
    if !on {
        return match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(file.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
    std::fs::write(file, desktop_entry(&exe)).map_err(|e| e.to_string())
}

/// Registers or removes the login item.
#[cfg(not(all(free_unix, not(target_os = "linux"))))]
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
        overdesk_proto::log::warn("could not record the login default");
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
    fn the_autostart_entry_quotes_the_path() {
        let e = desktop_entry(Path::new(r#"/usr/local/bin/My "Desk" 100%"#));
        assert!(e.contains(r#"Exec="/usr/local/bin/My \"Desk\" 100%%""#), "{e}");
        assert!(e.starts_with("[Desktop Entry]\n"));
        assert!(e.contains(" --background\n"), "starts hidden at login: {e}");
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
