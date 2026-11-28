#!/usr/bin/env bash
# Build release + package the Free Overdesk app for the host OS with cargo-packager.
# Needs: cargo install cargo-packager --locked. Output: target/packages/
set -euo pipefail
cd "$(dirname "$0")/.."
# Fails the release if the binary needs anything beyond the OS's own libraries, so an
# installer never asks the user to install a runtime. Allow-list per OS; see docs/PACKAGING.md.
MACOS_MIN=11.0
check_deps() { # <binary>
  local bin=$1 deps bad
  case "$(uname -s)" in
  Darwin)
    deps=$(otool -L "$bin" | tail -n +2 | awk '{print $1}')
    bad=$(grep -Ev '^(/usr/lib/|/System/Library/)' <<<"$deps" || true)
    local minos
    minos=$(vtool -show-build "$bin" | awk '$1 == "minos" {print $2}')
    [ "$minos" = "$MACOS_MIN" ] || bad="$bad"$'\n'"minos $minos, expected $MACOS_MIN"
    ;;
  Linux)
    # glibc and libgcc only; the window-system libraries are dlopen()ed (bundled or declared in packager.json).
    deps=$(readelf -d "$bin" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
    bad=$(grep -Ev '^(lib(c|m|dl|pthread|rt|util|gcc_s)\.so\.[0-9]+|ld-linux-[a-z0-9_-]+\.so\.[0-9]+)$' <<<"$deps" || true)
    local have max=${MAX_GLIBC:-2.35} # Ubuntu 22.04: build the release there
    have=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -Vu | tail -n 1)
    [ "$(printf '%s\n%s\n' "$max" "$have" | sort -V | tail -n 1)" = "$max" ] || bad="$bad"$'\n'"needs glibc $have, baseline is $max"
    ;;
  *)
    # Windows (Git Bash). dumpbin from Visual Studio, else llvm-objdump (rustup component llvm-tools).
    if command -v dumpbin >/dev/null; then deps=$(dumpbin /dependents "$bin")
    elif command -v llvm-objdump >/dev/null; then deps=$(llvm-objdump -p "$bin")
    else echo "check_deps: need dumpbin or llvm-objdump" >&2; return 1; fi
    deps=$(grep -oiE '[a-z0-9_.-]+\.dll' <<<"$deps" | tr 'A-Z' 'a-z' | sort -u)
    # Windows' own DLLs only: never vcruntime140, msvcp140 or the like (crt-static in .cargo/config.toml).
    bad=$(grep -Ev '^(api-ms-win-.*|(kernel32|kernelbase|user32|gdi32|advapi32|shell32|shlwapi|ole32|oleaut32|ntdll|bcrypt|ncrypt|crypt32|secur32|ws2_32|mswsock|iphlpapi|dnsapi|userenv|dwmapi|shcore|comctl32|comdlg32|imm32|winmm|opengl32|propsys|uxtheme|setupapi|cfgmgr32|version|netapi32|rpcrt4|combase|d3d11|d3d12|dxgi|dcomp|d3dcompiler_47|user32|powrprof|wtsapi32|winspool|msimg32|dbghelp|ucrtbase)\.dll)$' <<<"$deps" || true)
    ;;
  esac
  bad=$(grep -v '^$' <<<"$bad" || true)
  if [ -n "$bad" ]; then echo "Release self-check failed for $bin:" >&2; echo "$bad" >&2; return 1; fi
  echo "Release self-check passed: $bin needs only OS libraries."
}
# The binary inside each rpm / pacman tarball is checked again: it is what users get.
check_packaged() { # <dir of packages>
  local f tmp
  tmp=$(mktemp)
  for f in "$1"/*.rpm "$1"/overdesk_*.tar.gz; do
    [ -f "$f" ] || continue
    case $f in # bsdtar (libarchive-tools) reads rpm
    *.rpm) bsdtar -xOf "$f" ./usr/bin/overdesk >"$tmp" ;;
    *) tar -xzOf "$f" --wildcards '*usr/bin/overdesk' >"$tmp" ;;
    esac
    check_deps "$tmp"
  done
  rm -f "$tmp"
}
VERSION=$(cargo pkgid -p overdesk-app | sed 's/.*[#@]//')
# Oldest macOS the code supports: 11.0, the first release for Apple silicon and the default Rust uses for aarch64 macOS. Nothing in the code needs newer; see docs/PACKAGING.md.
export MACOSX_DEPLOYMENT_TARGET=$MACOS_MIN
cargo build --release -p overdesk-app
case "$(uname -s)" in Darwin|Linux) EXE="";; *) EXE=.exe;; esac
check_deps "$PWD/target/release/overdesk$EXE"
cd crates/overdesk-app
# Version comes from Cargo (workspace-inherited). Config is passed as raw JSON: cargo-packager 0.11.8 fails to load config *files* (it chdirs into the file path).
case "$(uname -s)" in Darwin) FORMATS=app,dmg;; Linux) FORMATS=appimage,deb,pacman;; *) FORMATS=nsis;; esac
cargo packager --release -f "$FORMATS" -c "$(sed "s/^{/{\"version\":\"$VERSION\",/" packager.json)"
# .rpm (Fedora, openSUSE, RHEL family): cargo-packager has no rpm format. Metadata is in this crate's Cargo.toml.
if [ "$(uname -s)" = Linux ]; then
  command -v cargo-generate-rpm >/dev/null || cargo install cargo-generate-rpm --locked
  (cd ../.. && cargo generate-rpm -p overdesk-app --payload-compress gzip -o target/packages/)
  check_packaged ../../target/packages
fi
echo "Packages in: $(cd ../.. && pwd)/target/packages"
ls ../../target/packages
