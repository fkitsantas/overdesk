# Wayland backend: design note

Status: built, compiled and unit-tested (on macOS, with the Linux cross checks of `scripts/check-cross.sh`; no Docker), not yet hand-tested on a real GNOME or Plasma desktop. Code:
`crates/overdesk-platform/src/linux/{wayland.rs, wayland_core.rs}`; `ffi.rs` picks X11 or Wayland at run time. Clean
room: portal, libei and crate documentation only.

## How Wayland differs

Wayland gives an app no view of the pointer outside its own windows, and no way to grab input. Overdesk uses the
desktop portals instead, per user and without root (no evdev or uinput):

- **Capture**: [`org.freedesktop.portal.InputCapture`](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html).
  The app sets *pointer barriers* on screen edges. The compositor starts capture when the pointer crosses one and
  sends `Activated` (activation id, cursor position, barrier id). While active, input arrives over an EIS connection
  (libei receiver). `Release(activation_id, cursor_position)` ends it; the position is a hint. `Disabled` and
  `ZonesChanged` mean barriers must be set and capture enabled again.
- **Injection**: [`org.freedesktop.portal.RemoteDesktop`](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)
  through its `Notify*` methods (see Injection below). Keys and buttons use evdev codes, scroll is smooth or discrete.
  Consent is asked once and kept with `persist_mode = 2` and the single-use `restore_token`.

## Engine contract: no change needed

The engine decides crossings from `LocalMove { dx, dy, x, y }` in `Mode::Local`: it moves only when the cursor sits on
an edge pixel (`x >= w-1 && dx > 0`, and so on) and the delta pushes outward. So no `Entered` variant and no
`set_edges` are needed:

- An InputCapture `Activated` becomes one ordinary `Captured::Moved { dx, dy, x, y }`: a unit push outward across the
  barrier's edge, with `x, y` on that edge of the primary screen (primary-local, like every other adapter).
- Barriers go on every outer edge of the primary zone (ids fixed per edge: left 1, right 2, top 3, bottom 4). An edge
  with no neighbour is harmless: `Switcher::move_by` clamps (`Move::Within`) and the engine stays `Local`. The engine
  never says so, so the adapter gives an activation 400 ms (`ARBITRATION_MS`) to be claimed by `set_swallow(true)`;
  otherwise it calls `Release`. The same path covers a local button being held (the engine ignores the crossing).
- Once claimed, EIS motion is forwarded as `Moved` with relative `dx, dy` and a dead-reckoned `x, y` clamped to the
  zones (the engine dead-reckons by itself while driving and ignores `x, y`).
- `set_swallow(false)` (return crossing, `Escape`, peer lost) maps to `Release(activation_id, position)`, with the
  position one pixel inside the entry edge. A `set_swallow(true)` with nothing captured, a compositor-side
  `Deactivated`/`Disabled`/`ZonesChanged` while claimed, and any portal or EIS failure all emit `Captured::Escape`.
  A drop guard also emits it when the capture task ends for any reason, so local input always comes back.
- `Disabled` and `ZonesChanged` re-arm (zones, barriers, enable). On ZonesChanged the session is disabled first
  (GNOME 46 needs that to change barriers).
- The escape chord is read from EIS keys (evdev code to Overdesk code through `keymap.rs`), also before the engine
  has claimed the activation.

Only the primary zone gets barriers: it is the engine's screen. Other monitors are not modelled; an edge another
monitor touches is skipped (the portal denies it anyway).

## Injection

RemoteDesktop with `persist_mode = 2`; the `Notify*` D-Bus methods carry the input (the design's `ConnectToEIS`
sender is not used, to avoid the sender-side device handshake; revisit if latency shows). Keys use evdev codes
(X11 keycode minus 8). Wheel notches go as discrete steps, trackpad pixels as smooth axis. The portal only moves
the pointer relatively here (absolute needs a ScreenCast stream), so absolute targets from the engine become deltas
from a tracked position. The capture release position seeds that tracker; with no known position the pointer is
first homed to the top-left corner. Hand-test this on multi-monitor layouts.

### Consent and restore token

The new `restore_token` is saved after every start (single use) in `<store dir>/wayland-restore-token`, mode 0600,
where the store dir is `OVERDESK_HOME` or `~/.overdesk`. A stale token is ignored by the portal, which asks again.
InputCapture consent is not persisted: its restore token exists only in portal interface v2 (`CreateSession2` and
`Start`), and GNOME 46 offers v1. Use v2 where available later.

### App id of an unsandboxed app

The portal documents `org.freedesktop.host.portal.Registry.Register(app_id)` for non-Flatpak apps: it associates the
D-Bus connection with an app id that "must be able to match the basename of a .desktop file", only once, before any
other portal call, and not for sandboxed apps. Overdesk calls it (ashpd `register_host_app`) with
`org.overdesk.Overdesk` and ignores a refusal. The portal docs do not say whether restore tokens work without an app
id, so this is a hand-test item: without it, expect the consent dialog on every start. The matching
`org.overdesk.Overdesk.desktop` is installed by the rpm, the AUR and FreeBSD and OpenBSD packages (the
source is `crates/overdesk-app/linux/org.overdesk.Overdesk.desktop`). The deb and AppImage come from
cargo-packager, which names its generated entry after the binary (`overdesk.desktop`), so there the
portal may find no match; whether that costs the consent dialog on every start is a hand-test item.

## Crates

`ashpd` 0.14.3 (features `tokio`, `input_capture`, `remote_desktop`, plus `screencast` which remote_desktop imports)
and `reis` 0.7.1 (`tokio`; pure-Rust libei, MIT), with `tokio` and `futures-util`. One process-wide thread runs a
current-thread runtime and a `LocalSet` (the EIS streams are not `Send`); it owns every portal session and bridges
to the adapter's sync channels. Runtime backend choice: `WAYLAND_DISPLAY` or `XDG_SESSION_TYPE=wayland` selects the
portals; if they are unavailable the error says so and that XWayland/X11 is not used (it reaches only XWayland
windows).

## Support and risks

- InputCapture: GNOME 45+ and Plasma 6.1+ (to confirm on hardware); no wlroots/Sway support found. RemoteDesktop with
  EIS: GNOME and Plasma 6.0+.
- The compositor, not Overdesk, decides when capture starts; the release position is only a hint.
- Absolute pointer motion needs a ScreenCast stream; relative motion with a tracked position is used (see Injection).
- Fractional scaling: zone coordinates differ from our pixels.
- The escape chord must be read from EIS keys while captured.
- Clipboard: receiving works everywhere. Sending works on KDE (6.7+), Sway (1.11+) and Hyprland, which have a
  data-control protocol (`ext-data-control-v1`, else `wlr-data-control`): the offered MIME types are read, and a copy
  marked `x-kde-passwordManagerHint` is never sent. GNOME has no such protocol for background apps, so a GNOME
  session fails closed: a password manager's copy cannot be told apart, so nothing is sent outward (receiving
  still works). The RemoteDesktop clipboard could close that gap later.

## Hand test (GNOME 46+, Plasma 6.1+, with a Mac or X11 peer)

1. First run: consent dialogs appear (RemoteDesktop, InputCapture); second run: no RemoteDesktop dialog, and
   `wayland-restore-token` has mode 0600 and a new value each start. Note whether it still asks without a `.desktop` id.
2. Push the pointer through each edge that has a peer neighbour: capture starts, local apps stop seeing input.
3. Push through an edge with no neighbour: the pointer comes back within about half a second.
4. Return crossing from the peer: pointer appears one pixel inside the entry edge; check `Release` honours the position.
5. Escape chord (Ctrl+Alt+Super+Esc) while captured: input returns at once.
6. Keys, buttons, scroll direction (wheel and trackpad), and no stuck modifier after a disconnect.
7. Plug or unplug a monitor, or change scale, while idle and while captured.
8. Kill the portal (`systemctl --user restart xdg-desktop-portal`) while captured: local input returns.
