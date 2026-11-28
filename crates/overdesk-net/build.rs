// SPDX-License-Identifier: AGPL-3.0-only
//! Defines `free_unix`: the Unix desktops with X11 or the freedesktop portals (Linux, FreeBSD,
//! OpenBSD). Cargo manifests cannot see custom cfgs, so those spell the list out.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(free_unix)");
    if matches!(
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("linux" | "freebsd" | "openbsd")
    ) {
        println!("cargo::rustc-cfg=free_unix");
    }
}
