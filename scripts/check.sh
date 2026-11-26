#!/usr/bin/env bash
# Everything that must pass before merging to main. Run on each OS you changed code for.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
if command -v cargo-deny >/dev/null; then cargo deny check; else echo "cargo-deny not installed: skipped license check"; fi
echo "All checks passed."
