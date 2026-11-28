# Hand tests (Free app, two computers)

**How to use.** These checks need real machines, so no unit test covers them. Work through one machine at a time and tick each box as you go. Every step has a *Setup* (what must be true first), exact *Steps*, an *Expected* result and *If it fails, record* (what to write down in the bug report). Always record the OS version, the Overdesk version and the exact wording of anything the app shows. Unless a section says otherwise, pair the machine under test with a Mac (see [README](../README.md)). For design background see [ARCHITECTURE.md](ARCHITECTURE.md), [PACKAGING.md](PACKAGING.md) and [WAYLAND.md](WAYLAND.md); for the evidence format of earlier hand tests see [tdd/daemon.tdd.md](tdd/daemon.tdd.md). Results go into a table like the one there (check, result, finding).

| Machine | Needed for |
|---|---|
| Windows 10/11 PC, a mouse, ideally a 150% scaled display | Section 1 |
| Linux, X11 session (also one without RandR if you can) | Section 2 |
| Linux, Wayland: KDE Plasma 6.1+, GNOME 46+, Sway or Hyprland | Section 2 |
| Mac with two displays and a trackpad (Magic Trackpad or built-in) | Section 3 |
| FreeBSD 14+ with Xorg; FreeBSD with KDE Plasma 6.1+ Wayland; OpenBSD with Xorg | Section 6 |
| Any OS, laptop plus a taller external display | Section 4 |
| Any two computers | Section 5 |

## 1. Windows PC (paired with a Mac)

- [ ] **1.1 Pair and cross.** Setup: both apps running, same network. Steps: pick the other computer under Nearby devices, match the six-digit codes, move the pointer off the shared edge and back. Expected: pointer appears on the other screen at the matching height, and returns. If it fails, record: the code shown on each side, which edge, any notice text.
- [ ] **1.2 Pointer speed 10.** Setup: Windows pointer speed at 10 of 11, "Enhance pointer precision" ON; drive the Mac from the PC mouse. Steps: move 10 cm on the mouse. Expected: Mac pointer travels about as far, relative to screen size, as a Windows pointer would. If it fails, record: speed setting, DPI of the mouse, rough distance on each screen.
- [ ] **1.3 Other speeds, precision off.** Setup: repeat 1.2 with pointer speed 6 (default), then 1 and 11, each with "Enhance pointer precision" ON and OFF. Steps: same 10 cm move. Expected: feel matches the PC's own pointer in every combination; with precision OFF the distance is linear. If it fails, record: the combination that feels wrong, and whether too fast or too slow.
- [ ] **1.4 Display scale 100% and 150%.** Setup: set the PC display to 100%, then 150%; sign out and in if Windows asks. Steps: cross to the Mac and back, then drive the Mac. Expected: crossing height is right and speed does not change with scale. If it fails, record: scale, display resolution, where the pointer landed.
- [ ] **1.5 Very slow movement.** Setup: driving the Mac from the PC mouse. Steps: push the mouse a few millimetres per second in one direction for 20 seconds, then move it back the same way. Expected: the Mac pointer ends where it started: no drift, no stuck axis. If it fails, record: direction, total drift in pixels, speed settings.
- [ ] **1.6 Clipboard to and from.** Steps: copy text on the PC, paste on the Mac; copy on the Mac, paste on the PC. Expected: both paste. If it fails, record: the text, direction, time between copy and paste.
- [ ] **1.7 Clipboard latency.** Steps: while crossed, copy text and paste on the other computer at once. Expected: arrives in about 100 ms, no polling delay. If it fails, record: the measured delay (stopwatch or phone video).
- [ ] **1.8 Password manager copy is not sent.** Setup: KeePassXC and 1Password. Steps: copy a password from each, then paste on the Mac. Expected: nothing arrives; the Mac clipboard keeps its old content. If it fails, record: which manager and its version, and whether its "clear clipboard" option is on.
- [ ] **1.9 Clipboard markers.** Setup: a tool that sets `ExcludeClipboardContentFromMonitorProcessing`, or `CanIncludeInClipboardHistory` = 0, on its copy (a password manager does this). Steps: copy with it; also copy plain text from Notepad. Expected: marked copy is not sent; plain text is, and shows in Win+V history. If it fails, record: the marker, the tool, what was sent.
- [ ] **1.10 No leftover threads.** Setup: open Task Manager, Details tab, add the Threads column, note Overdesk's count. Steps: pair, cross, copy, disconnect, repeat 5 times. Expected: thread count returns to about the first value, not rising each time. If it fails, record: counts after each round.
- [ ] **1.11 L-shaped monitors.** Setup: two PC monitors in an L (second below and to the right). Steps: see Section 4.

## 2. Linux

### X11

- [ ] **2.1 Pair and cross.** Steps: as 1.1. Expected: same. If it fails, record: distro, desktop, `echo $XDG_SESSION_TYPE`.
- [ ] **2.2 Clipboard both ways.** Steps: copy text each way; also a long paragraph. Expected: pastes. If it fails, record: the text length and app.
- [ ] **2.3 Password manager copy not sent.** Setup: KeePassXC (sets `x-kde-passwordManagerHint`). Steps: copy a password, paste on the Mac. Expected: nothing arrives. If it fails, record: manager and version.
- [ ] **2.4 X11 without RandR.** Setup: an X server without RandR (for example `Xvfb -extension RANDR`, or Xephyr with RandR off). Steps: start Overdesk. Expected: starts and uses the whole screen size, no crash. If it fails, record: the stderr output.

### Wayland (KDE Plasma 6.1+, GNOME 46+, Sway, Hyprland)

Run one session at a time. Follow the numbered list under "Hand test" in [WAYLAND.md](WAYLAND.md) too; record which desktop each result belongs to.

- [ ] **2.5 Portal consent.** Setup: fresh `OVERDESK_HOME` (empty dir). Steps: start; accept the dialogs; quit; start again. Expected: consent dialogs the first time; the second start asks for no RemoteDesktop dialog; `wayland-restore-token` has mode 0600 and a new value each start. If it fails, record: desktop, whether it asked again, `ls -l` of the token.
- [ ] **2.6 Crossing.** Steps: push the pointer through an edge with a peer; push through an edge with none. Expected: first captures; second comes back within about half a second. If it fails, record: edge, desktop.
- [ ] **2.7 Clipboard in.** Steps: copy on the Mac, paste on Linux. Expected: pastes on all four desktops.
- [ ] **2.8 Clipboard out.** Steps: copy on Linux, paste on the Mac. Expected: works on KDE, Sway and Hyprland; on GNOME nothing is sent. If it fails, record: desktop and compositor version.
- [ ] **2.9 Password manager copy not sent (KDE Wayland).** Steps: copy from KeePassXC, paste on the Mac. Expected: nothing arrives. If it fails, record: manager version.
- [ ] **2.10 First move after homing.** Setup: Wayland receiving input, pointer position unknown (just started the app). Steps: have the Mac drive it; make the first move toward the centre of the Linux screen. Expected: pointer ends where the move says, not offset by the homed corner. If it fails, record: layout, first move, where it landed.

### Tray and key store (X11 and Wayland)

- [ ] **2.11 Tray host present.** Setup: KDE, XFCE or GNOME with the AppIndicator extension. Steps: start, close the window. Expected: tray icon stays; app keeps running. If it fails, record: desktop, extensions.
- [ ] **2.12 Tray host absent.** Setup: stock GNOME without the extension. Steps: start, close the window. Expected: the app quits. If it fails, record: process still running?
- [ ] **2.13 Key created in Secret Service.** Setup: fresh `OVERDESK_HOME=<dir>`, unlocked GNOME Keyring or KWallet. Steps: start, quit, run `secret-tool search store <dir>`. Expected: one item found; no `key.der` in `<dir>`. If it fails, record: keyring, command output, `ls <dir>`.
- [ ] **2.14 Existing `key.der` migrates.** Setup: a dir with `key.der` from an older run; note the fingerprint shown in the app. Steps: start with the new build. Expected: fingerprint unchanged; `key.der` gone; item in Secret Service. If it fails, record: both fingerprints, stderr.
- [ ] **2.15 Locked or headless keyring.** Setup: lock the keyring, or run with no session bus. Steps: start. Expected: still starts; `key.der` (mode 0600) is used. Unlock and restart: key moves into the keyring, same fingerprint. If it fails, record: stderr, file mode.
- [ ] **2.16 Two stores are distinct.** Steps: start with `OVERDESK_HOME=/tmp/a`, then `/tmp/b`. Expected: two different fingerprints and two Secret Service items. If it fails, record: both fingerprints.
- [ ] **2.17 KWallet.** Setup: KDE with KWallet as Secret Service. Steps: repeat 2.13. Expected: same. If it fails, record: KDE version and wallet prompts.

## 3. Mac

- [ ] **3.1 Display plugged and unplugged.** Setup: paired and crossing. Steps: plug a second display in mid-session, cross; unplug it. Expected: no crash; crossing still lands at the right height. If it fails, record: which display, console log.
- [ ] **3.2 Double-click speed.** Setup: set System Settings, Mouse, double-click speed to fast, then slow. Steps: drive the PC from the Mac and double-click a file. Expected: the double-click follows the Mac setting. If it fails, record: setting, outcome.
- [ ] **3.3 High-resolution wheel.** Setup: a mouse with a free-spinning or high-resolution wheel. Steps: nudge the wheel by tiny steps while driving the other computer. Expected: small steps scroll small amounts; none are lost. If it fails, record: mouse model, lines scrolled per click.
- [ ] **3.4 Smart zoom probe.** Setup: Terminal, repo root. Steps: `cargo run -p overdesk-platform --example mac_probe -- listen`, then two-finger double-tap on the trackpad. Expected: the probe prints an event. Record always: the event type and every field printed (this settles the open smart-zoom item in `mac/ffi.rs`).
- [ ] **3.5 Pinch event type.** Steps: same probe, pinch in and out over Preview. Expected: it prints type 29, 30 or both. Record always: which. If both appear, the pinch is counted twice and one type must be dropped (`shortcut:` in `mac/ffi.rs`).
- [ ] **3.6 Caps Lock before crossing.** Steps: turn Caps Lock on, cross to the other computer, type letters. Expected: they come out capital there. If it fails, record: the other OS and the letters typed.
- [ ] **3.7 Display change between sessions.** Setup: paired. Steps: disconnect, change this Mac's resolution (or plug a display in), connect again, cross to the other computer and back. Expected: the Desk map shows the new size and crossing lands at the right height. A change *during* a session is only picked up at the next connection. If it fails, record: old and new resolution.
- [ ] **3.8 macOS 11 or 12 (only if you have one).** Steps: install the official build, grant Input Monitoring and Accessibility, pair and cross. Expected: it runs like on a current macOS (the build targets 11.0, see [PACKAGING.md](PACKAGING.md)). If it fails, record: macOS version, the exact error, console log.

## 4. Multi-monitor (do on each OS)

Setup: a laptop screen beside a taller external display, so there are empty areas (holes) at top-left and bottom-left of the layout. Place the peer to the left.

- [ ] **4.1 Leave off a hole-facing edge.** Steps: move the pointer off the laptop's left edge at the height of the top hole, then the bottom hole. Expected: it does not cross, and the pointer stays on screen. If it fails, record: OS, layout sizes, where it went.
- [ ] **4.2 Arrive into a hole.** Steps: cross back from the peer at heights that face each hole. Expected: pointer lands on the nearest monitor. If it fails, record: the height entered and where it appeared.
- [ ] **4.3 L-shape (Windows).** Steps: repeat 4.1 and 4.2 with monitors in an L. Expected: same.

## 5. Desk map

- [ ] **5.1 Drag to arrange (Pro only; skip in the Free app, where the map is read-only).** Steps: drag a screen on the map to a different side of the other. Expected: layout changes and crossing now uses the new side. If it fails, record: start and end sides.
- [ ] **5.2 Click without moving.** Steps: click a screen without moving the mouse. Expected: nothing is rearranged.
- [ ] **5.3 Release off the map.** Steps: start a drag, release outside the map. Expected: the drag is cancelled; layout unchanged.
- [ ] **5.4 Long names.** Setup: rename a computer to a 60-character name. Steps: look at the map and the lists. Expected: the name is cut with "…" and never overflows its box. If it fails, record: screenshot.

## 6. FreeBSD and OpenBSD

Neither has been run yet: the code is cross-linted for FreeBSD from a Mac only, and OpenBSD (Rust tier 3) has never been compiled. Pair each machine with a Mac. Record the OS version, the output of `pkg info overdesk` (FreeBSD) or `pkg_info overdesk` (OpenBSD), and the driver stack (`pkg info | grep xf86-input`).

- [ ] **6.1 Build and start (FreeBSD, X11).** Setup: Xorg session with a desktop or window manager. Steps: install the port or the `.pkg`, run `overdesk`. Expected: the window opens with this computer's hostname as its name. If it fails, record: stderr, `ldd /usr/local/bin/overdesk`.
- [ ] **6.2 Crossing and typing (X11).** Steps: pair with the Mac as 1.1, cross both ways, type letters, digits, punctuation, arrows, Ctrl and Shift shortcuts. Expected: all arrive on the right keys. The key table is the evdev one; a console keyboard driver (`xf86-input-keyboard`) may number keys differently, so record which input driver is in use. If it fails, record: the keys that came out wrong and `xinput list`.
- [ ] **6.3 Clipboard both ways (X11).** Steps: copy text each way, a long paragraph, and an image from the Mac. Expected: pastes (the image only where Pro is installed). If it fails, record: text length, application.
- [ ] **6.4 Password manager copy not sent.** Steps: as 2.3. Expected: nothing arrives.
- [ ] **6.5 Tray.** Setup: a desktop with a StatusNotifier host (KDE, XFCE with the plugin). Steps: as 2.11 and 2.12. Expected: same. If it fails, record: desktop, plugins.
- [ ] **6.6 Key store.** Steps: as 2.13 to 2.15 with GNOME Keyring or KWallet from packages, then with neither running. Expected: Secret Service item when one is reachable; otherwise `key.der` with mode 0600. If it fails, record: keyring, `ls -l <dir>`, stderr.
- [ ] **6.7 Open at login.** Steps: turn "Open at login" on in the tray menu, look for `~/.config/autostart/org.overdesk.Overdesk.desktop`, log out and in. Expected: the file exists and the app starts after login on a desktop that reads XDG autostart; turning it off removes the file. If it fails, record: desktop, file content.
- [ ] **6.8 Device name and peer label.** Steps: pair with the Mac. Expected: the Mac lists this machine by its hostname, with the label "Linux" (the BSDs announce Linux on the wire). If it fails, record: the name shown.
- [ ] **6.9 Wayland on FreeBSD KDE.** Setup: FreeBSD with KDE Plasma 6.1+ and `xdg-desktop-portal` plus `xdg-desktop-portal-kde`, a Wayland session. Steps: follow 2.5 to 2.10. Expected: same as Linux. If it fails, record: portal packages and versions, `echo $XDG_SESSION_TYPE`, stderr.
- [ ] **6.10 Wayland on OpenBSD gives a clear error.** Setup: OpenBSD with any Wayland compositor (if you can run one). Steps: start. Expected: an error that says Wayland is not supported on OpenBSD and to use an X11 session; no crash. If it fails, record: the text.
- [ ] **6.11 OpenBSD build and X11 use.** Steps: `doas pkg_add rust`, `cargo build --release -p overdesk-app`, run 6.1 to 6.8 in an Xorg session. Expected: builds and behaves as FreeBSD. If it fails, record: the first compiler or linker error (note the crate), and the Rust version.
- [ ] **6.12 No self-update.** Steps: open the tray menu. Expected: only Show, Disconnect, Open at login and Quit; the app never offers a new version. If it fails, record: what it showed.

## 7. Pairing, forgetting and login (any OS)

- [ ] **7.1 Forget tells the other computer.** Setup: two paired computers, connected. Steps: on one, click Forget next to the other and confirm. Expected: the other shows "<name> forgot this computer. Pair again to reconnect.", lists nothing as paired, and does not reconnect. If it fails, record: what the other computer showed.
- [ ] **7.2 Forget asks first, from both lists.** Steps: click Forget on a row under Nearby devices, and on one under Paired devices. Expected: both ask "Forget <name>? You'll need to pair again." and Cancel changes nothing. If it fails, record: which list.
- [ ] **7.3 Open at login starts hidden.** Setup: an installed build with "Open at login" on. Steps: log out and in. Expected: no window opens; the tray icon is there (with no tray, the window opens instead); "Show Overdesk" brings the window. If it fails, record: OS and what appeared.
- [ ] **7.4 Names survive a restart.** Steps: pair, quit both apps, start only one. Expected: under Paired devices the other computer has its name, not a string of hex digits. If it fails, record: what is shown.
