#!/usr/bin/env bash
# Build release + package Overdesk for the host OS with cargo-packager.
# Needs: cargo install cargo-packager --locked. Output: target/packages/
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -p overdesk-app
VERSION=$(cargo pkgid -p overdesk-app | sed 's/.*[#@]//')
cd crates/overdesk-app
# Version comes from Cargo (workspace-inherited). Config is passed as raw JSON: cargo-packager 0.11.8 fails to load config *files* (it chdirs into the file path).
case "$(uname -s)" in Darwin) FORMATS=app,dmg;; *) FORMATS=nsis;; esac
cargo packager --release -f "$FORMATS" -c "$(sed "s/^{/{\"version\":\"$VERSION\",/" packager.json)"
echo "Packages in: $(cd ../.. && pwd)/target/packages"
ls ../../target/packages
