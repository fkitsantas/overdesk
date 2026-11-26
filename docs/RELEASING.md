# Releasing

Each release is a signed build that installed apps pick up by themselves. Run this on each OS you ship (macOS, Windows and Linux builds cannot be cross-made).

## One-time setup

- `cargo install cargo-packager --locked`
- The release signing key lives **outside the repo** in `~/.config/overdesk-release/` on the release machine:
  - `updater.key`: the private key (never commit it, never copy it into CI logs).
  - `updater.pass`: its password. Alternatively export `CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD` (and `CARGO_PACKAGER_SIGN_PRIVATE_KEY` for a different key path or its contents).
  - `updater.key.pub`: the public key; a copy is committed as `crates/overdesk-app/updater.pub` and compiled into the app.
- Back the key and password up somewhere safe. If the key is lost or replaced, installed apps (which trust only the old public key) can no longer be updated and users must reinstall. To rotate, ship one update signed with the old key that contains the new `updater.pub`.
- Regenerate (only if you must): `cargo packager signer generate --path ~/.config/overdesk-release/updater.key --password <pw> --ci`, then copy `updater.key.pub` to `crates/overdesk-app/updater.pub`.

## Cutting a release

1. Bump `version` in the workspace `Cargo.toml`; commit and tag.
2. `scripts/package.sh` builds, packages and signs into `target/packages/` (`Overdesk_<v>_<arch>.app.tar.gz` + `.sig` on macOS, renamed before signing so the signed file name carries the version, which the app requires to refuse downgrades; `Overdesk_<v>_<arch>.AppImage.tar.gz` + `.sig` on Linux (also renamed before signing; `.deb` and `.AppImage` are for first installs); `Overdesk_<v>_<arch>-setup.exe` + `.sig` on Windows; the `.dmg` is for first installs).
3. `scripts/update-manifest.sh <out-dir> https://overdesk.org/updates/files` writes `<out-dir>/<target>/<arch>/latest.json` and copies the artifact and `.sig` next to it.
4. Upload `<out-dir>` so the artifact URLs in `latest.json` resolve. Upload the installer (`.dmg` / `-setup.exe`) for new users.
5. Make `https://overdesk.org/updates/<target>/<arch>/<current_version>` answer with that `latest.json` for every older version (a server rewrite of `/updates/:target/:arch/:version` to `latest.json` is simplest). Apps already on the latest version compare equal and do nothing. Answering 204 is also fine.
6. Rollback hand test (macOS): install 0.1.0, offer 0.1.1 built to crash at startup, restart into it, and after three launches expect 0.1.0 back and `bad-updates` listing 0.1.1. If a release is broken, ship a newer fixed version: installed apps skip a version only after it failed on that Mac.
7. Check with an installed older build: `OVERDESK_UPDATE_URL=... ` and look for `overdesk: update X downloaded and verified` on stderr.

Manifest shape (the crate's dynamic format): `{"version":"0.1.1","url":"https://.../Overdesk_0.1.1_aarch64.app.tar.gz","signature":"<contents of the .sig file>","format":"app"}` (`"nsis"` on Windows). `target` is `macos`, `windows` or `linux` (Linux: `"format":"appimage"`, and the file is the signed `Overdesk_<v>_<arch>.AppImage.tar.gz`); `arch` is `aarch64` or `x86_64`.

Testing without publishing: `OVERDESK_VERSION=0.1.1 scripts/package.sh`, serve the manifest directory with `/usr/bin/python3 -m http.server`, and run an older packaged app with `OVERDESK_UPDATE_URL='http://127.0.0.1:8000/{{target}}/{{arch}}/{{current_version}}'`.
