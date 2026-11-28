# Overdesk architecture

Overdesk is designed from first principles around one experience: **every device on your
desk feels like one computer, and Apple input feels native everywhere.**

## 1. Desk of equal peers

There is no server and no client. Every device runs the same `overdesk` daemon. Paired
devices form a **Desk**.

- **Input follows the hand.** Whichever device's physical keyboard, mouse or trackpad you
  touch becomes the *source*. The device that holds the cursor is the *focus*.
- **Arbitration: last touch wins.** After the source changes, it is held for 300 ms so that
  two people bumping two mice don't fight over the cursor.
- Every peer runs the same pure `Switcher` on the shared layout, so any peer can compute
  focus changes.

```mermaid
flowchart LR
    subgraph Desk
        A["🍎 MacBook<br/>source"] -- motion lane --> B["🪟 Windows PC<br/>focus"]
        A -. control lane .- B
        A -. control lane .- C["🐧 Linux box"]
        B -. control lane .- C
    end
```

## 2. Two lanes, one QUIC connection

Each pair of peers shares one QUIC connection (TLS 1.3) with two lanes:

| Lane | QUIC primitive | Carries | Why |
|---|---|---|---|
| **Motion** | Unreliable datagrams | Absolute pointer position plus a sequence number | A lost packet is simply superseded by the next one, so the cursor never stalls. Stale or reordered datagrams are dropped by sequence number (serial-number arithmetic, wrap-safe). |
| **Control** | One reliable, ordered stream | Focus, keys, buttons, scroll, gestures, clipboard, Desk state | Order matters: a key-down must never arrive after its key-up. |

Control frames are a `u32` big-endian length followed by a [postcard](https://docs.rs/postcard)
payload. Frames over 1 MiB are refused on both sides. A motion datagram is one bare postcard
message.

## 3. Replicated Desk state

Layout, peers and settings live on **every** peer as one versioned document:

- An edit bumps the version and broadcasts on the control lane. The open core never edits it
  after a session starts: the dialer authors the two-screen Desk once. Rearranging screens,
  by dragging on the Desk map, is a Pro feature (`AppState::can_arrange`).
- Peers keep the highest `(version, author)`.
- There is no master configuration file.

A received document is adopted only if:
- its `author` is the authenticated peer that sent it;
- its version moves forward by at most 2²⁰ (honest peers bump it by 1 per edit);
- it has at most 64 screens;
- every screen belongs to a peer this device has paired with;
- the layout itself is valid, with no overlaps and no zero sizes.

The document carries layout only, never trust.

## 4. Semantic input model

Events describe *intent*, not raw OS events. Each OS adapter translates to and from its native APIs.

| Event | Encoding |
|---|---|
| Keys | USB HID usage IDs. Command/Control swap between Mac and PC peers. |
| Pointer | Absolute position, in the focused screen's logical, DPI-independent coordinates |
| Scroll | Pixel or ¹⁄₁₂₀-notch deltas plus a phase (`Began`, `Changed`, `Ended`, `Momentum…`), so macOS inertia and rubber-banding survive the trip |
| Gestures | Magnify, rotate and smart zoom, with phases, delivered natively on macOS. Two-finger double-tap and two-finger swipe are sent once, by name (what the fingers did), to a peer that announced `FEATURE_GESTURES`; the receiver maps each to a shortcut of its own (a picker on Windows and Linux/BSD, the Mac's own Mission Control and Space shortcuts on a Mac). A Mac announces `FEATURE_MAGIC_MOUSE` and `FEATURE_TRACKPAD` in `Hello` when it has them, so the receiver can show the picker |

Apple fidelity lives in the macOS adapter. The wire format never throws that information away.

## 5. Hexagonal layering

```mermaid
flowchart TB
    UI["overdesk-app<br/>iced, pure Rust"] --> CORE
    D["overdesk daemon"] --> CORE
    CORE["overdesk-core<br/>pure logic: layout · switch · remap · held keys"] --> PROTO["overdesk-proto<br/>wire types"]
    CORE -. trait .- PLAT["overdesk-platform<br/>macOS · Windows · Linux/BSD adapters"]
    CORE -. trait .- NET["overdesk-net<br/>QUIC lanes · pairing · discovery"]
```

- `core` and `proto` never touch the OS or the network. They are fully unit-tested.
- Platform and network code are adapters behind traits, each with a fake implementation for tests.

## 6. Lazy clipboard

A peer whose clipboard changes **announces** the formats it holds (text, image, files).
Bytes cross the wire only when another peer actually pastes, so copying a 50 MB image
costs nothing until it's used.

As shipped: text still travels inline as `Control::Clipboard`. Images and files use
**offer / get**: the owner sends `ClipOffer { id, kind, items }` (names and sizes only),
the receiver answers `ClipGet { id }` when the user pastes, and either side may send
`ClipCancel { id }`. The bytes then move on a **transfer stream**: a QUIC uni stream
that starts with the 16-byte `id` followed by raw content and ends with the stream's
finish. It is separate from the control stream and capped at one at a time, with a
connection window of four frames against two per stream, so a transfer cannot starve
input. Nodes accept transfer streams only when bound with `Node::bind_with_transfers`;
a default node grants no uni-stream credit, so a peer cannot open one at all.

The open core ships only this plumbing (message types, transfer streams, platform
clipboard image/file access). **Free is text-only**: its daemon ignores offers and never
offers anything. Sharing images and files is **Pro**, implemented outside this repository.

## 7. Pairing

Each device has a long-lived key pair inside a self-signed certificate. Its `PeerId` is
the first 16 bytes of the SHA-256 fingerprint of that certificate.

1. The devices connect over mutual TLS 1.3. Certificate *chains* are not checked, but the
   handshake signature always is, so a device must hold the private key of the
   certificate it presents.
2. On a connection between peers that aren't yet trusted, only the `Pairing` message
   type exists. `Control` messages never decode there, so an unpaired device cannot send
   input.
3. **Commit, then reveal:**
   - The dialer sends `Commit = SHA-256(dialer fingerprint ‖ dialer nonce)`.
   - The listener replies with its own fresh nonce.
   - The dialer reveals its nonce, and the listener checks it against the commitment.
4. Both screens show `SHA-256(both fingerprints ‖ both nonces) mod 10⁶` as a 6-digit code.

Because the dialer commits before it sees the listener's nonce, an attacker in the middle
can't keep trying keys until the codes match. It gets one chance in a million per
attempt, and every failed attempt is visible to the user.

5. Each user confirms that the codes match. The peer is pinned only after **both** sides
   confirm. Later connections are authenticated by the pinned fingerprint, with no prompt.

Trust is strictly pairwise: replicated Desk state can never add a trusted key.

### Limits against unpaired peers

Any device on the network can complete a TLS handshake with its own self-signed
certificate. Before pairing, it can only:

- open **one** stream, and send frames of at most 128 bytes;
- take up at most a few MB of buffered data;
- hold its setup for 5 s, and a pairing for 60 s, including the user's answer.

On top of that:
- Unknown source addresses must first prove they're real (QUIC Retry).
- At most 64 handshakes can be in progress at once.
- Control and motion messages are refused until the peer's fingerprint is pinned.

Also built into the daemon:
- Pairing is accepted only while the user has "Pair a device" open, and only one at a time.
- Repeated failed pairings back off: after 3 failures, inbound pairing is refused for 10 s, then 30 s, then 60 s. The count is global and survives closing the window; a successful pairing resets it. Your own outbound dials are never blocked.

## 8. Install once, update without admin

The goal: IT approves and installs Overdesk once, and every later update of the official app installs itself
for that user, silently, with no admin prompt. That's what makes Overdesk viable on
managed corporate laptops.

**Rules that all code follows now:**

- **Device private key in OS storage:** the macOS Keychain, Windows DPAPI (per user) or the Linux and BSD Secret Service (GNOME Keyring / KWallet). Without a usable Secret Service (headless), a 0600 file.
- **Per-user only.** No Windows service, no `launchd` daemon, no root helper, no writes
  to `Program Files`, `/Library` or `HKLM`. Settings and keys live under the user's
  profile (already done: `~/Library/Application Support/Overdesk` on macOS,
  `%APPDATA%\Overdesk` on Windows).
- **Start at login per-user:** a user-owned LaunchAgent in `~/Library/LaunchAgents` on macOS
  (the LaunchAgent mode of `auto-launch`: it needs no signed bundle and no Automation prompt),
  the `HKCU\…\Run` key on Windows, and an XDG autostart file in `~/.config/autostart` on
  Linux and the BSDs. Every one passes `--background`, so the app starts hidden in the tray.
- **Input access without admin:**
  - On Windows, low-level hooks and `SendInput` work as a standard user. Windows won't let
    a standard process inject into elevated (admin) windows; we document that and don't
    work around it.
  - On macOS, the Accessibility and Input Monitoring grants belong to the code signature.
    Updates signed with the same Developer ID keep the grants, so they're not prompted for again.

**Install locations (user-writable, so the app can replace itself):**

| OS | Installer | Location |
|---|---|---|
| Windows | Per-user NSIS installer (cargo-packager, `currentUser` install mode) | `%LOCALAPPDATA%\Overdesk` |
| macOS | `.dmg` holding `Overdesk.app`, dragged to `~/Applications` | `~/Applications/Overdesk.app` |
| Linux | AppImage (per user, no install); `.deb`, `.rpm` and pacman packages install system-wide (`/usr/bin`) and need the package manager's privileges | AppImage: wherever the user keeps it; packages: `/usr/bin/overdesk` |
| FreeBSD, OpenBSD | `pkg` or the ports tree; no self-update | `/usr/local/bin/overdesk` |

Self-update ships in the official app only; builds from source update by rebuilding.

**For IT departments:**
- **Code signing:** Authenticode on Windows, Developer ID with notarization on macOS. IT
  can then allow-list Overdesk *by publisher* (AppLocker or WDAC publisher rules, Jamf or
  Intune), so updates in a per-user folder still pass.

**Costs to plan for:**
- Apple Developer Program: about £79 a year.
- A Windows code-signing certificate: about £200–400 a year.

## Status

| Part | State |
|---|---|
| Wire types: control and motion lanes, `PeerId`, focus, gestures | ✅ |
| Wrap-safe sequence filter for motion | ✅ |
| `Switcher`, held keys, ⌘↔Ctrl remap | ✅ |
| Replicated Desk state (merge and validation) | ✅ |
| Lazy clipboard: text inline; image and file offer / get with transfer streams (Free is text only, images and files are Pro) | ✅ |
| Pairing exchange (commit-reveal, 6-digit code) | ✅ |
| QUIC lanes: mutual TLS 1.3, control stream, motion datagrams | ✅ |
| Saving device keys to disk, daemon | ✅ |
| LAN discovery (mDNS) | ✅ |
| Per-user installers (§8) | ✅ built; install test pending |
| Platform adapters | ✅ macOS (hand-tested); ✅ Windows, Linux (X11 and Wayland), FreeBSD and OpenBSD (X11) built and tested in software, real-hardware tests pending |
