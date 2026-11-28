// SPDX-License-Identifier: AGPL-3.0-only
//! Linux, FreeBSD and OpenBSD adapter (`free_unix`). `ffi` picks the backend at run time: `x11` (XInput2 and XTest) on Xorg, or
//! `wayland` (xdg-desktop-portal InputCapture and RemoteDesktop with libei) on Wayland. Per
//! user, never root: no evdev or uinput, which would need a udev rule or group membership
//! (ARCHITECTURE §8). The pure tables and the Wayland state machine compile and are tested on
//! every OS; only the display-server I/O is `free_unix`-only (OpenBSD: X11 only).

/// Pure X11 input semantics (buttons, scroll clicks).
pub mod events;
/// Evdev-based X11 keycode table.
pub mod keymap;
/// Pure keysym choice (level, group, dead keys) for the X11 character lookup.
pub mod keysym;
/// Pure Wayland logic: capture state machine, barriers, pointer and scroll translation.
pub mod wayland_core;

#[cfg(free_unix)]
pub mod ffi;
#[cfg(free_unix)]
pub mod layout;
#[cfg(free_unix)]
mod wayland;
#[cfg(free_unix)]
mod x11;
