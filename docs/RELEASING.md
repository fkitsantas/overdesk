# Releasing

Each release is a signed build that installed apps pick up by themselves. Run this on each OS you ship (macOS and Windows builds cannot be cross-made).

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
2. `scripts/package.sh` builds, packages and signs into `target/packages/` (`Overdesk.app.tar.gz` + `.sig` on macOS, `Overdesk_<v>_<arch>-setup.exe` + `.sig` on Windows; the `.dmg` is for first installs).
3. `scripts/update-manifest.sh <out-dir> https://overdesk.org/updates/files` writes `<out-dir>/<target>/<arch>/latest.json` and copies the artifact and `.sig` next to it.
4. Upload `<out-dir>` so the artifact URLs in `latest.json` resolve. Upload the installer (`.dmg` / `-setup.exe`) for new users.
5. Make `https://overdesk.org/updates/<target>/<arch>/<current_version>` answer with that `latest.json` for every older version (a server rewrite of `/updates/:target/:arch/:version` to `latest.json` is simplest). Apps already on the latest version compare equal and do nothing. Answering 204 is also fine.
6. Check with an installed older build: `OVERDESK_UPDATE_URL=... ` and look for `overdesk: update X downloaded and verified` on stderr.

Manifest shape (the crate's dynamic format): `{"version":"0.1.1","url":"https://.../Overdesk_0.1.1_aarch64_Overdesk.app.tar.gz","signature":"<contents of the .sig file>","format":"app"}` (`"nsis"` on Windows). `target` is `macos` or `windows`; `arch` is `aarch64` or `x86_64`.

Testing without publishing: `OVERDESK_VERSION=0.1.1 scripts/package.sh`, serve the manifest directory with `/usr/bin/python3 -m http.server`, and run an older packaged app with `OVERDESK_UPDATE_URL='http://127.0.0.1:8000/{{target}}/{{arch}}/{{current_version}}'`.
