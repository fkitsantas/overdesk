<div align="center">

# Overdesk

**One keyboard. One mouse. Every computer on your desk.**

Slide your cursor off your Mac and keep working on your Windows PC,
with input that feels native on both.

[![CI](https://github.com/fkitsantas/Overdesk/actions/workflows/ci.yml/badge.svg)](https://github.com/fkitsantas/Overdesk/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![Commercial license](https://img.shields.io/badge/commercial-available-8A2BE2.svg)](COMMERCIAL.md)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg?logo=rust)](rust-toolchain.toml)
![Platforms](https://img.shields.io/badge/platforms-macOS%20·%20Windows%20·%20Linux%20·%20Android%20TV-lightgrey.svg)

> [!WARNING]
> **Pre-alpha.** The protocol and core logic exist and are tested. Nothing moves a real
> cursor yet. Star or watch the repo to follow along.

</div>

---

## Why Overdesk

Your desk is one workspace, not a server with clients bolted on. Overdesk turns every
device on it into a single computer, and it's built for the most common desk of all:
**a Mac beside a Windows PC.**

<table>
<tr>
<td width="50%" valign="top">

### ✋ Input follows your hand
There's no server or client. Touch the Mac trackpad and it drives the PC; grab the PC
mouse and it drives the Mac. Every device is an equal peer.

</td>
<td width="50%" valign="top">

### 🍎 Apple input, native everywhere
Pixel-precise Magic Mouse and trackpad scrolling, with momentum and rubber-banding intact.
Pinch, rotate and smart zoom. <kbd>⌘</kbd>+<kbd>C</kbd> copies on Windows.

</td>
</tr>
<tr>
<td valign="top">

### ⚡ A cursor that never stalls
Pointer motion travels as QUIC datagrams where the newest wins, so a lost packet is
simply superseded. Keys travel on a separate reliable lane and never arrive out of order.

</td>
<td valign="top">

### 🔐 Private by default
Every connection is TLS 1.3. To pair, both screens show the same 6-digit code. No
fingerprint files to copy around.

</td>
</tr>
<tr>
<td valign="top">

### 🧭 One Desk, edited anywhere
Layout and settings are replicated to every device. Rearrange your screens from whichever
computer you're sitting at.

</td>
<td valign="top">

### 📋 Clipboard on demand
A copy only announces what it holds. Bytes move when you paste, so a 50 MB screenshot
costs nothing until you use it.

</td>
</tr>
</table>

<sub>These features describe the design; the roadmap below shows what's built.</sub>

## Roadmap

| Phase | What | Status |
|---|---|---|
| 0 | Workspace, CI on macOS / Windows / Linux, license checks, CLA | ✅ Done |
| 1 | Two-lane wire protocol and core logic: screen layout, edge switching, held keys, ⌘↔Ctrl remap | ✅ Done |
| 2 | **macOS** capture and injection with native Magic Mouse / trackpad / keyboard feel | ✅ Built and hand-tested ([evidence](docs/tdd/macos-adapter.tdd.md)) |
| 3 | QUIC lanes, 6-digit pairing, replicated Desk state, latency benchmark | ✅ Built and tested over loopback |
| 3b | LAN discovery, saving device keys to disk, daemon that wires it all together | ⏳ Next |
| 4 | **Windows** capture and injection | 🔜 |
| 5 | Desktop app: Tauri 2 + Svelte 5, drag-to-arrange screens, tray icon | 🔜 |
| 6 | Linux (X11, then Wayland through libei) | 🔜 |
| 7 | Signed installers (`.dmg`, `.msi`, `.AppImage`) and Overdesk Pro license keys | 🔜 |
| 8 | Android TV client (Pro) | 🔭 Stretch |

## Architecture

```mermaid
flowchart LR
    subgraph Desk["Your Desk: equal peers"]
        A["🍎 MacBook<br/>source"] -- "motion lane · QUIC datagrams" --> B["🪟 Windows PC<br/>focus"]
        A <-. "control lane · reliable stream" .-> B
    end
```

| Crate | Role |
|---|---|
| [`overdesk-proto`](crates/overdesk-proto) | Wire types for both lanes, length-prefixed control frames (1 MiB cap), wrap-safe motion sequencing |
| [`overdesk-core`](crates/overdesk-core) | Pure logic: layout validation, cursor focus across screen edges, held-key release, ⌘↔Ctrl remap |
| [`overdesk-net`](crates/overdesk-net) | QUIC transport: mutual TLS 1.3 with device certificates, control stream and motion datagrams, 6-digit commit-reveal pairing, `Hello` identity checks |
| [`overdesk-platform`](crates/overdesk-platform) | OS adapters. macOS: Quartz event tap and injection with real modifier flags, drag events, double-click counts and scroll momentum phases |

Read the full design in **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

## Build from source

Requires [Rust](https://rustup.rs). `rust-toolchain.toml` selects the latest stable toolchain.

```bash
git clone https://github.com/fkitsantas/Overdesk.git
cd Overdesk
cargo test --workspace
```

On a Mac, try the input probe. First grant your terminal **Input Monitoring** and
**Accessibility** in System Settings › Privacy & Security:

```bash
cargo run -p overdesk-platform --example mac_probe -- listen   # prints your input for 10 s
cargo run -p overdesk-platform --example mac_probe -- inject   # draws a small square with the cursor
```

Measure motion-lane latency over loopback QUIC:

```bash
cargo run --release -p overdesk-net --example latency
# motion round trip over loopback, 2000 samples: p50 256µs  p99 804µs  lost 0   (measured on a Mac, release build)
```

## Contributing

Pull requests are welcome. Before your first one is merged, the CLA bot asks you to sign the
[Contributor License Agreement](CLA.md) by leaving a single comment.

Overdesk is a clean-room project. Please don't copy code from Deskflow, Synergy, Barrier or
Input Leap.

## Free and Pro

| | **Overdesk Free** | **Overdesk Pro** · £20 lifetime |
|---|:---:|:---:|
| Devices on one Desk | 2 | Unlimited |
| Native Magic Mouse, trackpad and keyboard feel | ✅ | ✅ |
| Encrypted peer-to-peer, text clipboard | ✅ | ✅ |
| File and image clipboard | — | ✅ |
| Trackpad gestures (pinch, rotate, smart zoom) | — | ✅ |
| Android TV | — | ✅ |

One payment, licensed to your email, for every device you own. No subscription.

## License

Overdesk is **open core**. Everything in this repository is
**[AGPL-3.0-only](LICENSE)**. Pro features are a separate, closed module. Companies that
want to embed Overdesk code in closed-source products can get a
**[commercial license](COMMERCIAL.md)**.

<div align="center"><sub>Made for people who own more than one computer and only want one keyboard.</sub></div>
