// SPDX-License-Identifier: AGPL-3.0-only
//! Linux adapter. `ffi` talks to the display server: X11 first, then Wayland through the
//! xdg-desktop-portal InputCapture and RemoteDesktop portals (libei). Per user, never root:
//! no evdev or uinput, which would need a udev rule or group membership (ARCHITECTURE §8).

#[cfg(target_os = "linux")]
pub mod ffi;
