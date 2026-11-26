// SPDX-License-Identifier: AGPL-3.0-only
//! Linux adapter. `ffi` picks the backend at run time: `x11` (XInput2 and XTest) on Xorg, or
//! `wayland` (xdg-desktop-portal InputCapture and RemoteDesktop with libei) on Wayland. Per
//! user, never root: no evdev or uinput, which would need a udev rule or group membership
//! (ARCHITECTURE §8). The pure tables and the Wayland state machine compile and are tested on
//! every OS; only the display-server I/O is Linux-only.

/// Pure X11 input semantics (buttons, scroll clicks).
pub mod events;
/// Evdev-based X11 keycode table.
pub mod keymap;
/// Pure Wayland logic: capture state machine, barriers, pointer and scroll translation.
pub mod wayland_core;

#[cfg(target_os = "linux")]
pub mod ffi;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;
