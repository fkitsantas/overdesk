# Packaging

Uses [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) (pure Rust, v0.11.8).

    cargo install cargo-packager --locked
    scripts/package.sh        # release build + package for the host OS -> target/packages/

- macOS: `Overdesk.app` and `Overdesk_<version>_<arch>.dmg`.
- Windows: NSIS installer, `installMode: currentUser` (per-user, never needs admin; see ARCHITECTURE.md section 8). Must be built on Windows; not tested from macOS.
- Linux: `Overdesk_<version>_<arch>.AppImage` (per user, self-updating, no install) and `overdesk_<version>_<arch>.deb` (system install under `/usr`). Build on Linux: `FORMATS=appimage,deb` (see `scripts/package.sh`, which currently picks `nsis` on non-macOS: pass the formats by hand with `cargo packager --release -f appimage,deb` from `crates/overdesk-app` until the script learns Linux). Icons and the `.desktop` entry (name, `Utility` category, icon) come from `packager.json`; the PNGs in `assets/brand/icons/` are installed into the hicolor theme.
  - The tray uses the StatusNotifierItem D-Bus protocol (tray-icon's `ksni` backend), so neither package needs GTK or libayatana-appindicator. KDE, XFCE, Cinnamon, MATE and others show it; stock GNOME needs the "AppIndicator and KStatusNotifierItem Support" extension. With no tray host the app skips the icon and closing the window quits.
  - The .deb `Depends`: `libxkbcommon0`, `libwayland-client0`, `libxdo3` (window system and input libraries loaded at run time; confirm `libxdo3` against the input crate in use).
  - Open at login writes `~/.config/autostart/org.overdesk.Overdesk.desktop` (an AppImage registers its own path from `$APPIMAGE`, never the temporary mount).
- Config: `crates/overdesk-app/packager.json` (a JSON file, passed inline by the script because cargo-packager 0.11.8 cannot load config files: it chdirs into the file path). Version is read from Cargo.
- macOS Info.plist additions (`NSLocalNetworkUsageDescription`, `NSBonjourServices` `_overdesk._udp`, `LSMinimumSystemVersion` 13.0) live in `crates/overdesk-app/macos/Info.plist` and are merged in.
- Icons: PNGs in `assets/brand/icons/` (from `app-icon-macos-1024.png`; one icon list for all platforms). The 1024 file must be named `...512@2x.png` or the icns step fails.

## Update signing

`scripts/package.sh` also signs the update artifacts (`Overdesk.app.tar.gz.sig` on macOS, `*-setup.exe.sig` on Windows) with the release key, which lives outside the repo at `~/.config/overdesk-release/updater.key`. cargo-packager 0.11.8 has no packager.json setting for this: signing is on whenever `CARGO_PACKAGER_SIGN_PRIVATE_KEY` (a key file or its contents) is set, and the script sets it from that default path. The password comes from `CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD`, or `~/.config/overdesk-release/updater.pass`. The matching public key is `crates/overdesk-app/updater.pub`, compiled into the app. `OVERDESK_VERSION=x.y.z scripts/package.sh` builds and packages a different version without touching `Cargo.toml` (for update tests). See [RELEASING.md](RELEASING.md).

## Self-update behaviour

- The app checks `https://overdesk.org/updates/{{target}}/{{arch}}/{{current_version}}` at startup and every 6 hours (override: `OVERDESK_UPDATE_URL`, same `{{...}}` variables). The response is the crate's dynamic manifest (see RELEASING.md), or 204 for "up to date".
- A found update is downloaded and its signature verified; on failure it is discarded silently and retried at the next check.
- A verified update is only staged. The idle screen then shows "Overdesk X is ready. Restart to update." with a Restart button; nothing installs by itself, so an update can never replace the app mid-session. The Restart button exists only on the idle screen, and the install is refused while pairing, connecting, driving or being driven.
- Tray menu: "Check for updates" (brings the window forward if one is ready).
- Linux: only the AppImage updates itself (the updater replaces the file named by `$APPIMAGE` in place, so the file must be writable by the user). A .deb is updated by the package manager; the updater would fail there, so it is skipped when `APPIMAGE` is not set.
- macOS: if the app's folder is not writable by the user the check is skipped (the crate would otherwise ask for an administrator password). Dev builds outside a `.app` never update.
- IT can turn updates off with `OVERDESK_DISABLE_UPDATES=1`, a macOS managed-preferences file `/Library/Managed Preferences/org.overdesk.Overdesk.plist` with `DisableUpdates` = true (read with `plutil`, XML or binary), or on Windows the DWORD `HKLM\Software\Policies\Overdesk\DisableUpdates` = 1.

## Signing limits

- macOS builds are ad-hoc signed (`signingIdentity: "-"`), not notarized. The identity changes on every build, so macOS permissions (Input Monitoring, Accessibility, Local Network) may need re-granting after each rebuild until a Developer ID certificate is used. Downloaded builds need right-click > Open.
- Windows installers are unsigned; SmartScreen will warn until an Authenticode certificate is configured.
