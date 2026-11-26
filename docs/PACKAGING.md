# Packaging

Uses [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) (pure Rust, v0.11.8).

    cargo install cargo-packager --locked
    scripts/package.sh        # release build + package for the host OS -> target/packages/

- macOS: `Overdesk.app` and `Overdesk_<version>_<arch>.dmg`.
- Windows: NSIS installer, `installMode: currentUser` (per-user, never needs admin; see ARCHITECTURE.md section 8). Must be built on Windows; not tested from macOS.
- Config: `crates/overdesk-app/packager.json` (a JSON file, passed inline by the script because cargo-packager 0.11.8 cannot load config files: it chdirs into the file path). Version is read from Cargo.
- macOS Info.plist additions (`NSLocalNetworkUsageDescription`, `NSBonjourServices` `_overdesk._udp`, `LSMinimumSystemVersion` 13.0) live in `crates/overdesk-app/macos/Info.plist` and are merged in.
- Icons: PNGs in `assets/brand/icons/` (from `app-icon-macos-1024.png`; one icon list for all platforms). The 1024 file must be named `...512@2x.png` or the icns step fails.

## Signing limits

- macOS builds are ad-hoc signed (`signingIdentity: "-"`), not notarized. The identity changes on every build, so macOS permissions (Input Monitoring, Accessibility, Local Network) may need re-granting after each rebuild until a Developer ID certificate is used. Downloaded builds need right-click > Open.
- Windows installers are unsigned; SmartScreen will warn until an Authenticode certificate is configured.
