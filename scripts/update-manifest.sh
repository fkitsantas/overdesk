#!/usr/bin/env bash
# Write the update manifest for the artifact that scripts/package.sh just built (host OS).
# Usage: scripts/update-manifest.sh <out-dir> <artifact-base-url>
# Produces <out-dir>/<target>/<arch>/latest.json and copies the artifact + .sig beside it.
# Upload <out-dir>; see docs/RELEASING.md for how the per-version URL maps to latest.json.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${1:?out dir}; BASE=${2:?artifact base url, e.g. https://overdesk.org/updates/files}
VERSION=${OVERDESK_VERSION:-$(cargo pkgid -p overdesk-app | sed 's/.*[#@]//')}
case "$(uname -s)" in
  Darwin) TARGET=macos; FORMAT=app; ART="Overdesk.app.tar.gz";;
  *) TARGET=windows; FORMAT=nsis; ART=$(cd target/packages && ls Overdesk_"${VERSION}"_*-setup.exe);;
esac
case "$(uname -m)" in arm64|aarch64) ARCH=aarch64;; *) ARCH=x86_64;; esac
SRC=target/packages/$ART
[ -f "$SRC.sig" ] || { echo "missing $SRC.sig: package with the signing key first" >&2; exit 1; }
DEST="$OUT/$TARGET/$ARCH"; mkdir -p "$DEST"
NAME="Overdesk_${VERSION}_${ARCH}_${ART}"
cp "$SRC" "$DEST/$NAME"; cp "$SRC.sig" "$DEST/$NAME.sig"
printf '{"version":"%s","url":"%s/%s/%s/%s","signature":"%s","format":"%s"}\n' \
  "$VERSION" "$BASE" "$TARGET" "$ARCH" "$NAME" "$(tr -d '\n' < "$SRC.sig")" "$FORMAT" > "$DEST/latest.json"
echo "wrote $DEST/latest.json"
