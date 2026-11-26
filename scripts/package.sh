#!/usr/bin/env bash
# Build release + package Overdesk for the host OS with cargo-packager, and sign the
# update artifacts. Needs: cargo install cargo-packager --locked. Output: target/packages/
#
# Env:
#   OVERDESK_VERSION                          override the version (release tests; default: Cargo's)
#   CARGO_PACKAGER_SIGN_PRIVATE_KEY           key file or contents (default: ~/.config/overdesk-release/updater.key)
#   CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD  its password (default: contents of ~/.config/overdesk-release/updater.pass)
# With no key available the build still packages, but unsigned (no .sig files, no updates).
set -euo pipefail
cd "$(dirname "$0")/.."
KEYDIR="$HOME/.config/overdesk-release"
if [ -z "${CARGO_PACKAGER_SIGN_PRIVATE_KEY:-}" ] && [ -f "$KEYDIR/updater.key" ]; then
  export CARGO_PACKAGER_SIGN_PRIVATE_KEY="$KEYDIR/updater.key"
fi
if [ -z "${CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD:-}" ] && [ -f "$KEYDIR/updater.pass" ]; then
  CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD="$(cat "$KEYDIR/updater.pass")"
  export CARGO_PACKAGER_SIGN_PRIVATE_KEY_PASSWORD
fi
# The app reads OVERDESK_VERSION at compile time (src/update.rs), so binary and package agree.
VERSION=${OVERDESK_VERSION:-$(cargo pkgid -p overdesk-app | sed 's/.*[#@]//')}
export OVERDESK_VERSION="$VERSION"
cargo build --release -p overdesk-app
cd crates/overdesk-app
# Version comes from Cargo (workspace-inherited). Config is passed as raw JSON: cargo-packager 0.11.8 fails to load config *files* (it chdirs into the file path).
case "$(uname -s)" in Darwin) FORMATS=app,dmg;; *) FORMATS=nsis;; esac
cargo packager --release -f "$FORMATS" -c "$(sed "s/^{/{\"version\":\"$VERSION\",/" packager.json)"
echo "Packages in: $(cd ../.. && pwd)/target/packages"
ls ../../target/packages
