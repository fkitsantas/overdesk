# Packaging

Uses [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) (pure Rust, v0.11.8).

    cargo install cargo-packager --locked
    scripts/package.sh        # release build + package for the host OS -> target/packages/

- macOS: `Overdesk.app` and `Overdesk_<version>_<arch>.dmg`.
- Windows: NSIS installer, `installMode: currentUser` (per-user, never needs admin; see ARCHITECTURE.md section 8). Must be built on Windows; not tested from macOS.
- Linux: `Overdesk_<version>_<arch>.AppImage` (per user, no install) `overdesk_<version>_<arch>.deb` (system install under `/usr`), `overdesk_<version>_<arch>.tar.gz` + `PKGBUILD` (pacman) and an `.rpm`.  Build on Linux (the script picks `appimage,deb,pacman`, then runs `cargo generate-rpm`). Icons and the `.desktop` entry (name, `Utility` category, icon) come from `packager.json`; the PNGs in `assets/brand/icons/` are installed into the hicolor theme.
  - The tray uses the StatusNotifierItem D-Bus protocol (tray-icon's `ksni` backend), so neither package needs GTK or libayatana-appindicator. KDE, XFCE, Cinnamon, MATE and others show it; stock GNOME needs the "AppIndicator and KStatusNotifierItem Support" extension. With no tray host the app skips the icon and closing the window quits.
  - The .deb `Depends` are listed in `packager.json` (see "What's inside the installers"); `libxdo3` was dropped because nothing uses it (input goes through x11rb/XTest).
  - Open at login writes `~/.config/autostart/org.overdesk.Overdesk.desktop` with `--background` (an AppImage registers its own path from `$APPIMAGE`, never the temporary mount).
  - The menu entry is `org.overdesk.Overdesk.desktop` in the rpm, AUR and BSD packages (the Wayland portals match the app id to that file name, see WAYLAND.md); cargo-packager's deb and AppImage name theirs `overdesk.desktop`.
- Config: `crates/overdesk-app/packager.json` (a JSON file, passed inline by the script because cargo-packager 0.11.8 cannot load config files: it chdirs into the file path). Version is read from Cargo.
- macOS Info.plist additions (`NSLocalNetworkUsageDescription`, `NSBonjourServices` `_overdesk._udp`, `LSMinimumSystemVersion` 11.0) live in `crates/overdesk-app/macos/Info.plist` and are merged in.
- Icons: PNGs in `assets/brand/icons/` (from `app-icon-macos-1024.png`; one icon list for all platforms). The 1024 file must be named `...512@2x.png` or the icns step fails.

## What's inside the installers

Nothing to install first: no Rust, no Visual C++ Redistributable, no .NET, no extra libraries by hand. `scripts/package.sh` runs `check_deps` on the release binary and fails the build if it needs anything outside the allow-list below.

- **Windows**: the C runtime is linked into the exe (`+crt-static` in `.cargo/config.toml`), so `vcruntime140.dll` is not needed. The exe imports only Windows' own DLLs (kernel32, user32, ...). wgpu uses DirectX 12 / Vulkan, which ship with Windows and the GPU driver. Allow-list checked with `dumpbin /dependents` or `llvm-objdump -p`; the check has only been written against the Windows tools' output format, not run on Windows yet.
- **macOS**: deployment target 11.0 (`MACOSX_DEPLOYMENT_TARGET` in `package.sh`, `minimumSystemVersion` and `LSMinimumSystemVersion` in the bundle config). 11.0 is the first macOS for Apple silicon and the default Rust itself uses for `aarch64-apple-darwin`. Nothing needs newer: open at login uses the LaunchAgent mode of `auto-launch` (a plist the user owns), not `SMAppService` (13.0+), and the system calls the adapter makes (`CGPreflight*EventAccess` and `CGRequest*EventAccess` are 10.15, event taps and `NSEvent` phases older, Metal 10.13, Local Network keys in `Info.plist` ignored before they exist) are older. It has only been built and run on the current macOS, so 11 and 12 are untested: the hand tests in HAND-TESTS.md section 3 are the way to find out. Only `/usr/lib` and `/System/Library` libraries are linked (`otool -L`), and `vtool -show-build` must say `minos 11.0`.
- **Linux**: the binary links only glibc and libgcc. The window-system libraries are loaded with `dlopen` (winit, xkbcommon-dl, wgpu), so `ldd` does not show them: the AppImage bundles libxkbcommon, libxkbcommon-x11, libX11, libX11-xcb, libXcursor, libXrandr, libXi and libwayland-client/cursor/egl (`appimage.libs`, found under `/usr/lib*` on the build machine, so install them there first), and the `.deb` declares them in `depends`. The GPU stack (libvulkan, libEGL, libGL, drivers) and glibc are never bundled: they belong to the system. The Secret Service client (zbus) and X11 protocol code (x11rb) are pure Rust. **Build releases on the oldest supported Ubuntu LTS (22.04, glibc 2.35)**: a binary needs at least the glibc it was built against. `check_deps` fails if the binary needs a newer glibc than `MAX_GLIBC` (default 2.35).

- **Linux, one format per distro family** (all built on Ubuntu 22.04, glibc 2.35, so every artifact runs on 22.04 and anything newer):

  | Distros | Artifact | How it gets its libraries |
  |---|---|---|
  | Ubuntu, Debian, Mint | `overdesk_<v>_<arch>.deb` | `deb.depends` in `packager.json` |
  | Fedora, openSUSE, RHEL family | `overdesk-<v>-1.<arch>.rpm` ([cargo-generate-rpm](https://github.com/cat-in-136/cargo-generate-rpm), metadata in `[package.metadata.generate-rpm]` of the app crate's `Cargo.toml`) | `requires` by soname, e.g. `libxkbcommon.so.0()(64bit)`: rpm resolves that on Fedora and openSUSE alike although their package names differ. cargo-generate-rpm also adds the glibc versions the binary needs. |
  | Arch, Manjaro | `overdesk_<v>_<arch>.tar.gz` + `PKGBUILD` (cargo-packager `pacman`; run `makepkg` beside them on Arch for the `.pkg.tar.zst`), and the AUR recipe in `packaging/aur` | `pacman.depends` in `packager.json` (Arch names: libxkbcommon, libxkbcommon-x11, libx11, libxcursor, libxrandr, libxi, wayland, libglvnd, vulkan-icd-loader) |
  | Anything else | `Overdesk_<v>_<arch>.AppImage` | bundles the window-system libraries (`appimage.libs`) |

  The rpm and pacman lists name the same libraries as the deb. pacman cannot express "vulkan or GL", so Arch depends on both (`libglvnd`, `vulkan-icd-loader`); the rpm requires `libEGL.so.1` only. Mesa and the GPU driver are never bundled.
- **Linux build machine**: Ubuntu 22.04 with the window-system libraries installed (for the AppImage `libs` lookup), `libarchive-tools` (`bsdtar`, reads the rpm for the second `check_deps`) and `cargo-generate-rpm` (`scripts/package.sh` runs `cargo install cargo-generate-rpm --locked` if it is missing). `check_deps` runs on the release binary and again on the binary extracted from the rpm and the pacman tarball.

## Signing limits

- macOS builds are ad-hoc signed (`signingIdentity: "-"`), not notarized. The identity changes on every build, so macOS permissions (Input Monitoring, Accessibility, Local Network) may need re-granting after each rebuild until a Developer ID certificate is used. Downloaded builds need right-click > Open.
- Windows installers are unsigned; SmartScreen will warn until an Authenticode certificate is configured.

The BSDs never self-update: pkg and the ports tree own the install. Self-update ships in the official app only (see the closed repo); builds from source update by rebuilding.

## Arch User Repository

`packaging/aur/` holds `PKGBUILD` and `.SRCINFO` for `overdesk`: it builds the Free app from the `v<version>` tag tarball on GitHub with cargo. Publishing is manual, by the owner:

1. Once: create an AUR account, add your SSH key, `git clone ssh://aur@aur.archlinux.org/overdesk.git`.
2. Per release, after the tag exists: set `pkgver` (and `pkgrel=1`) in `PKGBUILD`, run `updpkgsums`, test with `makepkg -si` on Arch, then `makepkg --printsrcinfo > .SRCINFO`. The checked-in `.SRCINFO` is hand-written for 0.1.0 with `sha256sums = SKIP`, so regenerate it.
3. Copy `PKGBUILD` and `.SRCINFO` into the AUR clone, commit, `git push`.

The official Pro build is `overdesk-bin` (closed repo, `packaging/aur-bin`).

## FreeBSD

A full platform: X11, and Wayland where `xdg-desktop-portal` with InputCapture and RemoteDesktop exists (KDE Plasma 6.1+). `scripts/check-cross.sh` runs clippy for `x86_64-unknown-freebsd` from a Mac (zig 0.17 as the C compiler for ring, `rustup target add x86_64-unknown-freebsd`); it type-checks and lints only, nothing is linked or run.

`packaging/freebsd/` is a ports-style port, `x11/overdesk`, for the Free app: `USES=cargo`, built from the `v<version>` tag on GitHub, `Makefile.crates` generated from `Cargo.lock`, `pkg-plist`, `pkg-descr`. It declares the window-system libraries as `RUN_DEPENDS` (libxkbcommon, libX11, libXcursor, libXrandr, libXi, wayland, mesa-libs) because they are `dlopen()`ed and `ldd` cannot see them. To build and test on FreeBSD:

1. Copy `packaging/freebsd` to `/usr/ports/x11/overdesk` (after tagging the release).
2. `make makesum` writes `distinfo` (none is checked in: it needs the real tag tarball).
3. After any `Cargo.lock` change, `make cargo-crates > Makefile.crates`.
4. `make stage && make package` produces the `.pkg`; `poudriere testport` for a clean-room run.

The official app with Pro is packaged by the closed repo's `scripts/package-freebsd.sh` on a FreeBSD build host (`pkg create`, with the same dependencies and an `ldd` allow-list of base-system libraries).

The BSD desktops share Linux's pieces: the Secret Service (with the 0600 `key.der` fallback), the StatusNotifierItem tray, and an XDG autostart file for "Open at login" (written by `crates/overdesk-app/src/login.rs`, since the `auto-launch` crate has no BSD backend). Cargo features and `cfg`s use `free_unix`, set by each crate's `build.rs` for Linux, FreeBSD and OpenBSD.

## OpenBSD

X11 only, a source build. Rust is tier 3 there (no prebuilt standard library), so nothing can be checked from a Mac and it has never been compiled by us. A Wayland session gives a clear error. On an OpenBSD machine:

    doas pkg_add rust git
    git clone https://github.com/fkitsantas/overdesk.git && cd overdesk
    cargo build --release -p overdesk-app     # target/release/overdesk

`packaging/openbsd/` is a skeleton for the ports tree (`x11/overdesk`): `Makefile` (`devel/cargo` module), `crates.inc` (from `Cargo.lock`), a placeholder `distinfo` (run `make makesum`), `pkg/PLIST` and `pkg/DESCR`. It is untested: expect to fix `WANTLIB` and the first build errors on a real machine, and report them as in HAND-TESTS section 6.

## Wire identity

The BSDs announce `Os::Linux` to peers. The wire enum is closed: a peer that has never heard of a new variant cannot decode the hello, so appending `FreeBsd` would break every installed app and the Android TV client until updated. The cost is that other computers label a BSD machine "Linux"; it is otherwise identical (PC modifier conventions).
