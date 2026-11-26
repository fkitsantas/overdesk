# Wayland backend: design note

Status: researched, not built. X11 comes first (`crates/overdesk-platform/src/linux`). Clean room: portal, libei and
crate documentation only.

## How Wayland differs

Wayland gives an app no view of the pointer outside its own windows, and no way to grab input. Overdesk uses the
desktop portals instead, per user and without root (no evdev or uinput):

- **Capture**: [`org.freedesktop.portal.InputCapture`](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html).
  The app sets *pointer barriers* on screen edges. The compositor starts capture when the pointer crosses one and
  sends `Activated` (activation id, cursor position, barrier id). While active, input arrives over an EIS connection
  (libei receiver). `Release(activation_id, cursor_position)` ends it; the position is a hint. `Disabled` and
  `ZonesChanged` mean barriers must be set and capture enabled again.
- **Injection**: [`org.freedesktop.portal.RemoteDesktop`](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)
  with `ConnectToEIS` (a libei sender). Keys and buttons use evdev codes, scroll is smooth or discrete. Consent is
  asked once and kept with `persist_mode = 2` and the single-use `restore_token` (save the new token every time).

## Contract change (other platforms unchanged)

The engine decides edge crossings from every local `Moved { x, y }`. Wayland has no such stream, so:

- `Capture::set_edges(&[Edge])`: the edges that have a neighbour. Wayland turns them into barriers; others ignore it.
- `Captured::Entered { edge, x, y }`: the pointer crossed a barrier. Only Wayland emits it; the engine treats it as
  "at the edge, start forwarding".
- While captured, `Moved` carries relative `dx, dy` from EIS, and `x, y` are dead-reckoned from the entry point.
- `set_swallow(false)` and `Capture::release_at(x, y)` map to `Release`. Elsewhere `release_at` is a no-op or a warp.

## Crates

`ashpd` 0.14.3 (portals, async on zbus/tokio, MIT) and `reis` 0.7.1 (pure-Rust libei protocol, MIT). One tokio thread
owns the portal sessions and bridges to the adapter's channels.

## Support and risks

- InputCapture: GNOME 45+ and Plasma 6.1+ (to confirm on hardware); no wlroots/Sway support found. RemoteDesktop with
  EIS: GNOME and Plasma 6.0+.
- The compositor, not Overdesk, decides when capture starts; the release position is only a hint.
- Absolute pointer motion over EIS may need a ScreenCast stream for its region; fall back to relative motion.
- Fractional scaling: zone coordinates differ from our pixels.
- The escape chord must be read from EIS keys while captured.
- Clipboard: GNOME has no data-control protocol for background apps (KDE 6.7, Sway 1.11 and Hyprland have
  `ext-data-control-v1`); on GNOME use the RemoteDesktop clipboard, or accept no clipboard.

## Hand test

GNOME 46+ and Plasma 6.x machines (or VMs) with a Mac or X11 peer. Check, in order: the `Activated` payload, whether
`Release` honours the position, the restore token surviving a restart, absolute motion without ScreenCast, key
mapping, scroll direction, the escape chord, and clipboard on GNOME.
