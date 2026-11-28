#!/usr/bin/env bash
# Clippy for Windows, Linux and FreeBSD from a Mac, without Docker. Needs `brew install zig` (the C
# compiler for ring's build script) and the Rust targets. Nothing is linked or run, so it
# catches compile and lint errors only; real-OS tests still need those machines.
set -euo pipefail
cd "$(dirname "$0")/.."
here="$PWD/scripts/cross"
lint() { # <rust target> <zig target>
  echo "== $1"
  local t="${1//-/_}"
  env "CC_$t=$here/zcc" "AR_$t=$here/zar" ZCC_TARGET="$2" \
    cargo clippy --workspace --all-targets --target "$1" -- -D warnings
}
lint x86_64-unknown-linux-gnu x86_64-linux-gnu
# MinGW headers stand in for MSVC's: ring only needs libc headers to build its objects.
lint x86_64-pc-windows-msvc x86_64-windows-gnu
# FreeBSD: a full platform. OpenBSD (Rust tier 3, no prebuilt std) cannot be checked from here.
lint x86_64-unknown-freebsd x86_64-freebsd
echo "Cross checks passed."
