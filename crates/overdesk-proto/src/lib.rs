// SPDX-License-Identifier: AGPL-3.0-only
//! Overdesk wire protocol. See `docs/ARCHITECTURE.md`.
//!
//! - Control lane: reliable stream of length-prefixed [`Control`] frames.
//! - Motion lane: unreliable datagrams, one bare [`Motion`] each, newest wins via [`SeqGate`].

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
pub mod hex;
pub mod log;
mod rect;

pub use rect::Rect;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Largest width or height, in pixels, a screen announced by a peer or placed on a Desk may have.
pub const MAX_SCREEN_SIDE: u32 = 65_535;

/// Wire protocol revision announced in [`Control::Hello`]. Bumped on any change that
/// makes old and new peers decode each other's frames differently. It stays at 1: later
/// additions are negotiated with the `FEATURE_*` bits of [`Control::Hello`] instead, so a
/// peer only sends what the other announced.
pub const PROTOCOL_VERSION: u16 = 1;
/// [`Control::Hello`] feature bit: the peer types [`Control::KeyText`] (an Android TV).
/// A sender must not send `KeyText` to a peer that did not announce it.
pub const FEATURE_KEY_TEXT: u64 = 1;
/// [`Control::Hello`] feature bit: the peer takes [`Control::Lease`] (an opaque signed statement
/// the official app exchanges). A sender must not send `Lease` to a peer that did not announce it.
pub const FEATURE_LEASE: u64 = 2;
/// Most bytes a [`Control::Lease`] may carry; larger is rejected on both encode and decode.
pub const MAX_LEASE_LEN: usize = 1024;
/// Frames above this are rejected before allocating, so a peer cannot exhaust memory.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;
/// Size in bytes of the big-endian `u32` length prefix in front of every control-lane frame.
pub const LEN_PREFIX: usize = 4;

/// Key codes are `page_selector | usage`: the low 16 bits hold a USB HID usage id and the
/// high 16 bits select its usage page. Selector 0 is the keyboard page (0x07), so plain
/// keyboard usages need no tag.
pub const PAGE_KEYBOARD: u32 = 0;
/// HID consumer page (0x0C): media, volume, brightness, eject.
pub const PAGE_CONSUMER: u32 = 0x0C << 16;
/// Apple vendor "top case" page (0xFF): Fn / Globe is usage 0x03.
pub const PAGE_APPLE_TOP_CASE: u32 = 0xFF << 16;

/// Stable device identity, derived from the device's public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PeerId(pub [u8; 16]);

impl PeerId {
    /// Lower-case hex of the id: a stable, collision-free name for files and screens.
    pub fn to_hex(&self) -> String {
        hex::hex(&self.0)
    }

    /// Inverse of [`PeerId::to_hex`]; `None` unless `s` is exactly 32 hex digits.
    pub fn from_hex(s: &str) -> Option<Self> {
        hex::unhex(s).map(Self)
    }
}

/// Operating system a peer runs. Receivers use it to remap modifier keys between the
/// Apple and PC keyboard conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    /// Apple macOS; Command is the primary shortcut modifier.
    MacOs,
    /// Microsoft Windows; Control is the primary shortcut modifier.
    Windows,
    /// Linux desktop; Control is the primary shortcut modifier.
    Linux,
    /// Android; follows the PC convention with Control as the shortcut modifier.
    Android,
}

/// Pointer button, independent of how the local OS numbers its buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Button {
    /// Primary button.
    Left,
    /// Secondary button, usually opening a context menu.
    Right,
    /// Middle button or pressed scroll wheel.
    Middle,
    /// Side button that navigates back (mouse button 4).
    Back,
    /// Side button that navigates forward (mouse button 5).
    Forward,
}

/// Units of a scroll delta. Apple Magic Mouse / trackpads report pixels; notched wheels lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollUnit {
    /// Logical pixels, as reported by high-resolution devices such as trackpads.
    Pixel,
    /// 1/120 of a notch, matching Windows `WHEEL_DELTA` granularity.
    Notch120,
}

/// Phase of a continuous scroll or gesture; lets the receiving Mac reproduce
/// rubber-banding and momentum (inertial) scrolling. Gestures never use `Momentum*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Discrete event outside any phase, such as one notch of a mouse wheel.
    None,
    /// First event of a continuous scroll or gesture: the fingers started moving.
    Began,
    /// Update within a scroll or gesture that has already begun.
    Changed,
    /// The fingers lifted and the scroll or gesture finished normally.
    Ended,
    /// The scroll or gesture was aborted, for example by the system.
    Cancelled,
    /// Fingers touched the trackpad but have not moved yet; stops running momentum.
    MayBegin,
    /// First synthetic inertial event after the fingers lifted.
    MomentumBegan,
    /// Inertial update while momentum scrolling decays.
    Momentum,
    /// Momentum scrolling came to rest or was interrupted.
    MomentumEnded,
}

/// Trackpad gestures, delivered natively on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Gesture {
    /// Pinch; scale delta in thousandths (`1000` = +100%).
    Magnify {
        /// Scale change in thousandths; negative values zoom out.
        milli: i32,
    },
    /// Rotation delta in thousandths of a degree.
    Rotate {
        /// Rotation change since the previous event, in thousandths of a degree.
        millideg: i32,
    },
    /// Two-finger double tap.
    SmartZoom,
}

/// Reliable, ordered control-lane message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// First message on a new connection: introduces the sending peer.
    Hello {
        /// Sender's [`PROTOCOL_VERSION`].
        version: u16,
        /// Sender's identity; must match the identity authenticated by TLS.
        peer: PeerId,
        /// Human-readable device name shown in the UI.
        name: String,
        /// Sender's operating system, used for modifier-key remapping.
        os: Os,
        /// Width of the sender's screen in logical pixels.
        width: u32,
        /// Height of the sender's screen in logical pixels.
        height: u32,
        /// Optional capabilities the sender supports, as `FEATURE_*` bits. Unknown bits are
        /// ignored, so a newer peer can announce more than an older one understands.
        features: u64,
    },
    /// Cursor focus moves to `peer`, at that peer's local logical coordinates. Motion
    /// datagrams of this focus period start at `seq`; older ones are stale.
    Focus {
        /// Peer that now holds the cursor.
        peer: PeerId,
        /// Horizontal entry point, local to `peer`'s screen.
        x: i32,
        /// Vertical entry point, local to `peer`'s screen.
        y: i32,
        /// First motion-lane sequence number that belongs to this focus period.
        seq: u32,
    },
    /// Carries the pointer position: the control lane is not ordered against motion
    /// datagrams, so a click must not depend on the last datagram having arrived.
    Button {
        /// Button that changed state.
        button: Button,
        /// `true` for press, `false` for release.
        pressed: bool,
        /// Pointer x at the time of the click, local to the focused screen.
        x: i32,
        /// Pointer y at the time of the click, local to the focused screen.
        y: i32,
    },
    /// Scroll wheel or trackpad scroll on the focused peer.
    Scroll {
        /// Horizontal delta in `unit`s, with the sign convention of the source device.
        dx: i32,
        /// Vertical delta in `unit`s, with the sign convention of the source device.
        dy: i32,
        /// Unit of `dx` and `dy`.
        unit: ScrollUnit,
        /// Position of this event within a continuous scroll.
        phase: Phase,
    },
    /// Trackpad gesture, reproduced natively when the focused peer is a Mac.
    Gesture {
        /// The gesture and its delta.
        gesture: Gesture,
        /// Position of this event within the gesture; never a `Momentum*` phase.
        phase: Phase,
    },
    /// `code` is `page_selector | HID usage`; see [`PAGE_KEYBOARD`].
    Key {
        /// Usage page selector ORed with the HID usage id, as typed on the source keyboard.
        code: u32,
        /// `true` for key-down, `false` for key-up.
        pressed: bool,
    },
    /// Replicated Desk layout (ARCHITECTURE §3).
    Desk(Desk),
    /// Interim: plain text. Replaced by lazy offer/request clipboard (ARCHITECTURE §6).
    Clipboard {
        /// Clipboard contents as UTF-8 text; bounded by [`MAX_FRAME_LEN`].
        text: String,
    },
    /// The sender copied non-text content and offers it. Nothing is sent until the
    /// receiver answers with [`Control::ClipGet`]. Free builds ignore it; sharing is a
    /// Pro feature (ARCHITECTURE §6).
    ClipOffer {
        /// Random id naming this offer; the transfer stream carries the same id.
        id: [u8; 16],
        /// What the offer holds.
        kind: ClipKind,
        /// The items on offer: one image, or the copied files.
        items: Vec<ClipItem>,
    },
    /// The receiver wants the content of offer `id`; the sender then opens a transfer stream.
    ClipGet {
        /// Id of the [`Control::ClipOffer`] being requested.
        id: [u8; 16],
    },
    /// Withdraws offer `id`, or abandons its transfer; either side may send it.
    ClipCancel {
        /// Id of the offer or transfer being dropped.
        id: [u8; 16],
    },
    /// Text the sender's own keyboard layout produced for a key press, for a receiver with
    /// no keyboard of its own (an Android TV), so any layout types correctly there.
    ///
    /// Routing rule: to such a receiver, a key-down that yields text is sent as `KeyText`
    /// *instead of* `Key` (never both, or the text would be typed twice); every other key
    /// (arrows, Enter, Backspace, Tab, Escape, modifiers, Ctrl/Cmd shortcuts, key-ups) is a
    /// plain [`Control::Key`]. A dead key sends nothing until it composes. Only a Pro hub
    /// sends it, and only to an Android peer; receivers drop a message that fails
    /// [`key_text_is_valid`].
    KeyText {
        /// The key's code as in [`Control::Key`], for logging and fallbacks.
        code: u32,
        /// The text to type: 1 to [`MAX_KEY_TEXT_CHARS`] characters, none a control character.
        text: String,
    },
    /// An opaque statement for the official app (bytes of at most [`MAX_LEASE_LEN`]; this
    /// crate never looks inside). Only sent to a peer that announced [`FEATURE_LEASE`]; the
    /// open controller ignores it.
    Lease(#[serde(deserialize_with = "lease_bytes", serialize_with = "check_lease_bytes")] Vec<u8>),
}

/// Whether a lease has a legal size: 1 to [`MAX_LEASE_LEN`] bytes.
fn lease_ok(b: &[u8]) -> bool {
    (1..=MAX_LEASE_LEN).contains(&b.len())
}

/// Serde hook: refuses to write a lease over [`MAX_LEASE_LEN`] or empty.
fn check_lease_bytes<S: serde::Serializer>(b: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
    if !lease_ok(b) {
        return Err(serde::ser::Error::custom("lease size out of range"));
    }
    b.serialize(s)
}

/// Serde hook: reads the bytes and refuses an empty or over-long lease.
fn lease_bytes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let b = Vec::<u8>::deserialize(d)?;
    if !lease_ok(&b) {
        return Err(serde::de::Error::custom("lease size out of range"));
    }
    Ok(b)
}

/// Most characters one [`Control::KeyText`] may carry (a base letter plus combining marks).
pub const MAX_KEY_TEXT_CHARS: usize = 8;

/// Whether `text` is acceptable in a [`Control::KeyText`]: non-empty, at most
/// [`MAX_KEY_TEXT_CHARS`] characters, and none that is a control character or hidden (the
/// invisible and text-reordering characters listed on `is_hidden`). Joiners (ZWJ, ZWNJ)
/// and variation selectors stay: emoji and many scripts need them. Receivers check this
/// because the text goes straight into a focused field.
pub fn key_text_is_valid(text: &str) -> bool {
    let mut n = 0;
    for c in text.chars() {
        n += 1;
        let bad = c.is_control() || is_hidden(c);
        if n > MAX_KEY_TEXT_CHARS || bad {
            return false;
        }
    }
    n > 0
}

/// The one list of characters that hide or reorder text, shared by [`key_text_is_valid`]
/// and [`is_invisible`] so the two cannot drift: the soft hyphen, direction marks (U+200E,
/// U+200F, U+061C), the zero-width space, line and paragraph separators and bidi
/// embeddings/overrides (U+2028-202E), word joiner, invisible operators and bidi isolates
/// (U+2060-206F), the BOM, interlinear annotation controls, tag characters
/// (U+E0000-E007F), noncharacters, the Mongolian vowel separator and the default-ignorable
/// fillers (U+034F, U+115F, U+1160, U+17B4, U+17B5, U+3164, U+FFA0). It excludes the
/// joiners ZWJ and ZWNJ, which emoji and many scripts need.
fn is_hidden(c: char) -> bool {
    let u = c as u32;
    matches!(
        u,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F
            | 0x1160
            | 0x17B4
            | 0x17B5
            | 0x180E
            | 0x200B
            | 0x200E
            | 0x200F
            | 0x2028..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFDD0..=0xFDEF
            | 0xFEFF
            | 0xFFA0
            | 0xFFF9..=0xFFFB
            | 0xE0000..=0xE007F
    ) || u & 0xFFFE == 0xFFFE
}

/// Characters that change how a name displays without being visible: everything hidden
/// for [`key_text_is_valid`] plus the joiners ZWJ and ZWNJ (U+200C, U+200D). Key text
/// keeps the joiners; names (devices, files) drop or reject them. Variation selectors are
/// allowed in both.
pub fn is_invisible(c: char) -> bool {
    is_hidden(c) || matches!(c, '\u{200C}' | '\u{200D}')
}

/// Longest [`ClipItem::name`] [`clip_name_is_valid`] accepts, in bytes (a common file
/// system limit).
pub const MAX_CLIP_NAME_BYTES: usize = 255;

/// Whether `name` is safe to use as a [`ClipItem::name`] when saving received content on
/// any OS: non-empty, at most [`MAX_CLIP_NAME_BYTES`] bytes, exactly one normal path
/// component, no separator (`/`, `\`), no `..`, none of the Windows-reserved characters
/// `: < > " | ? *`, no control or invisible character ([`is_invisible`], which rejects
/// the joiners too), not ending in `.` or a space, and not a reserved device name (CON,
/// PRN, AUX, NUL, CONIN$, CONOUT$, CLOCK$, COM0-9, LPT0-9 including the superscript digits,
/// any case, with or without an extension).
///
/// The wire format does not enforce this: it is the receiver's duty to check every item
/// before touching the file system, and to drop an offer that fails.
pub fn clip_name_is_valid(name: &str) -> bool {
    use std::path::{Component, Path};
    // Compared by chars: Windows also treats the superscript digits (1, 2, 3) after COM/LPT as
    // devices, and ignores trailing white space in the stem.
    let stem = name.split('.').next().unwrap_or("").trim_end().to_uppercase();
    let mut chars = stem.chars();
    let port = matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some('C'), Some('O'), Some('M')) | (Some('L'), Some('P'), Some('T'))
    ) && matches!((chars.next(), chars.next()), (Some('0'..='9' | '¹' | '²' | '³'), None));
    let device = port
        || matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
        );
    let mut parts = Path::new(name).components();
    let one_normal = matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None));
    one_normal
        && !device
        && name.len() <= MAX_CLIP_NAME_BYTES
        && !name.ends_with(['.', ' '])
        && !name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
        && !name.chars().any(|c| c.is_control() || is_invisible(c))
}

/// What a [`Control::ClipOffer`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipKind {
    /// One image.
    Image,
    /// One or more files.
    Files,
}

/// One item of a [`Control::ClipOffer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipItem {
    /// File name without any directory part, or a label for an image. Untrusted: the
    /// receiver must check it with [`clip_name_is_valid`] before using it as a path.
    pub name: String,
    /// Size in bytes of the item's content.
    pub size: u64,
}

/// One screen in the shared logical plane, owned by `peer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskScreen {
    /// Device that owns and displays this screen.
    pub peer: PeerId,
    /// Screen name, unique within the Desk.
    pub name: String,
    /// Left edge in global logical coordinates.
    pub x: i32,
    /// Top edge in global logical coordinates.
    pub y: i32,
    /// Width in logical pixels; must be non-zero.
    pub w: u32,
    /// Height in logical pixels; must be non-zero.
    pub h: u32,
}

/// The replicated Desk document. Carries layout only, never trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desk {
    /// Edit counter; together with `author` it orders competing documents.
    pub version: u64,
    /// Peer that made this edit; breaks ties between equal versions.
    pub author: PeerId,
    /// Every screen on the Desk; rectangles must not overlap.
    pub screens: Vec<DeskScreen>,
}

/// Pre-trust handshake (ARCHITECTURE §7). Its own type, so an unpaired peer cannot
/// send `Control` at all: those bytes never decode as pairing messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pairing {
    /// Dialer commits to its nonce before seeing the listener's.
    Commit {
        /// `SHA-256(dialer fingerprint || dialer nonce)`.
        digest: [u8; 32],
    },
    /// Listener's reply to [`Pairing::Commit`]: its own fresh nonce.
    Nonce {
        /// Random nonce chosen by the listener for this pairing attempt.
        nonce: [u8; 32],
    },
    /// Dialer opens its commitment; the listener checks it against the earlier digest.
    Reveal {
        /// The dialer's nonce that was hashed into [`Pairing::Commit`].
        nonce: [u8; 32],
    },
    /// The user confirmed that both screens show the same code.
    Confirm,
    /// The user declined, or the codes differed; the pairing attempt ends untrusted.
    Reject,
}

/// Motion-lane datagram: absolute pointer position on the focused screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Motion {
    /// Wrap-around sequence number; receivers drop anything not newer than the last.
    pub seq: u32,
    /// Pointer x in logical coordinates local to the focused screen.
    pub x: i32,
    /// Pointer y in logical coordinates local to the focused screen.
    pub y: i32,
}

/// Why a control-lane frame or motion datagram could not be encoded or decoded.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    /// Payload length in bytes exceeds [`MAX_FRAME_LEN`].
    #[error("frame length {0} exceeds limit {MAX_FRAME_LEN}")]
    TooLarge(usize),
    /// Bytes are not a valid postcard message, or trailing bytes follow it; carries detail.
    #[error("malformed message: {0}")]
    Malformed(String),
}

/// Maps a postcard error to [`FrameError::Malformed`], keeping its description.
fn malformed(e: postcard::Error) -> FrameError {
    FrameError::Malformed(e.to_string())
}

/// Encode one stream message as `u32 big-endian length || postcard payload`.
/// Fails with `TooLarge` rather than emit a frame the peer would reject.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    // The payload is written straight after a placeholder prefix: one buffer, no copy.
    let mut frame = postcard::to_extend(msg, vec![0; LEN_PREFIX]).map_err(malformed)?;
    let len = frame.len() - LEN_PREFIX;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    // The cast cannot truncate: the length was just bounded by MAX_FRAME_LEN.
    frame[..LEN_PREFIX].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(frame)
}

/// Decodes exactly one `T` from `bytes`; trailing bytes are [`FrameError::Malformed`].
fn exact<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    let (msg, rest) = postcard::take_from_bytes(bytes).map_err(malformed)?;
    if rest.is_empty() {
        Ok(msg)
    } else {
        Err(FrameError::Malformed(format!("{} trailing bytes", rest.len())))
    }
}

/// Decode one stream frame from the front of `buf`.
/// `Ok(None)` means more bytes are needed; `Ok(Some((msg, consumed)))` otherwise.
pub fn decode_frame<T: DeserializeOwned>(buf: &[u8]) -> Result<Option<(T, usize)>, FrameError> {
    let Some(prefix) = buf.first_chunk::<LEN_PREFIX>() else {
        return Ok(None);
    };
    let len = u32::from_be_bytes(*prefix) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let Some(payload) = buf.get(LEN_PREFIX..LEN_PREFIX + len) else {
        return Ok(None);
    };
    Ok(Some((exact(payload)?, LEN_PREFIX + len)))
}

/// Encode a motion datagram (no length prefix: the datagram is the frame).
pub fn encode_motion(m: &Motion) -> Vec<u8> {
    // Three varints into a Vec cannot fail.
    postcard::to_stdvec(m).expect("postcard serialization of Motion is infallible")
}

/// Decode one motion datagram. The datagram must hold exactly one [`Motion`]: missing
/// or trailing bytes are [`FrameError::Malformed`].
pub fn decode_motion(datagram: &[u8]) -> Result<Motion, FrameError> {
    exact(datagram)
}

/// Accepts only motion newer than the last accepted one, using wrap-safe
/// serial-number arithmetic so `seq` may overflow freely. Create one per connection:
/// a restarted sender reconnects, so it never meets a stale gate.
#[derive(Debug, Default)]
pub struct SeqGate {
    /// Highest sequence number accepted so far; `None` until the first one, which is
    /// then accepted unconditionally.
    last: Option<u32>,
}

impl SeqGate {
    /// Starts a new focus period: only `seq` and later are accepted from now on.
    pub fn start_at(&mut self, seq: u32) {
        self.last = Some(seq.wrapping_sub(1));
    }

    /// Records `seq` and returns `true` if it is newer than every sequence number accepted
    /// so far; returns `false` for duplicates and stale or reordered datagrams.
    pub fn accept(&mut self, seq: u32) -> bool {
        // RFC 1982 style: the wrapped difference read as signed is positive iff `seq` is
        // ahead of `last` by less than half the u32 space.
        let newer = self.last.is_none_or(|last| (seq.wrapping_sub(last) as i32) > 0);
        if newer {
            self.last = Some(seq);
        }
        newer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: PeerId = PeerId([7; 16]);

    fn all_controls() -> Vec<Control> {
        vec![
            Control::Hello {
                version: PROTOCOL_VERSION,
                peer: PEER,
                name: "mac-mini".into(),
                os: Os::MacOs,
                width: 2560,
                height: 1440,
                features: FEATURE_KEY_TEXT,
            },
            Control::Focus {
                peer: PEER,
                x: 0,
                y: 540,
                seq: 7,
            },
            Control::Button {
                button: Button::Left,
                pressed: true,
                x: 10,
                y: -3,
            },
            Control::Scroll {
                dx: 0,
                dy: -120,
                unit: ScrollUnit::Notch120,
                phase: Phase::None,
            },
            Control::Scroll {
                dx: -3,
                dy: 17,
                unit: ScrollUnit::Pixel,
                phase: Phase::Momentum,
            },
            Control::Gesture {
                gesture: Gesture::Magnify { milli: -250 },
                phase: Phase::Changed,
            },
            Control::Gesture {
                gesture: Gesture::SmartZoom,
                phase: Phase::None,
            },
            Control::Key {
                code: 0x04,
                pressed: false,
            },
            Control::Clipboard {
                text: "héllo 🌍".into(),
            },
            Control::ClipOffer {
                id: [9; 16],
                kind: ClipKind::Files,
                items: vec![ClipItem {
                    name: "a.txt".into(),
                    size: u64::MAX,
                }],
            },
            Control::ClipOffer {
                id: [1; 16],
                kind: ClipKind::Image,
                items: vec![],
            },
            Control::ClipGet { id: [2; 16] },
            Control::ClipCancel { id: [3; 16] },
            Control::KeyText {
                code: 0x1F,
                text: "é€ж".into(),
            },
            Control::Lease(vec![0xAB; MAX_LEASE_LEN]),
        ]
    }

    #[test]
    fn every_control_round_trips() {
        for msg in all_controls() {
            let frame = encode_frame(&msg).unwrap();
            assert_eq!(decode_frame::<Control>(&frame), Ok(Some((msg, frame.len()))));
        }
    }

    #[test]
    fn existing_variants_keep_their_encoding() {
        // Captured before ClipOffer/ClipGet/ClipCancel were appended; variant indices must not move.
        let enc = |m: &Control| encode_frame(m).unwrap();
        assert_eq!(enc(&Control::Key { code: 4, pressed: true }), [0, 0, 0, 3, 5, 4, 1]);
        let mut focus = vec![0, 0, 0, 21, 1];
        focus.extend([7; 16]);
        focus.extend([0, 184, 8, 7]);
        assert_eq!(
            enc(&Control::Focus {
                peer: PEER,
                x: 0,
                y: 540,
                seq: 7
            }),
            focus
        );
        let scroll = Control::Scroll {
            dx: 0,
            dy: -120,
            unit: ScrollUnit::Notch120,
            phase: Phase::None,
        };
        assert_eq!(enc(&scroll), [0, 0, 0, 6, 3, 0, 239, 1, 1, 0]);
        assert_eq!(
            enc(&Control::Clipboard { text: "hi".into() }),
            [0, 0, 0, 4, 7, 2, 104, 105]
        );
        let desk = Control::Desk(Desk {
            version: 1,
            author: PEER,
            screens: vec![],
        });
        let mut expected = vec![0, 0, 0, 19, 6, 1];
        expected.extend([7; 16]);
        expected.push(0);
        assert_eq!(enc(&desk), expected);
        // The new variants follow the last old one.
        assert_eq!(enc(&Control::ClipGet { id: [0; 16] })[4], 9);
        assert_eq!(enc(&Control::ClipCancel { id: [0; 16] })[4], 10);
        assert_eq!(
            enc(&Control::KeyText {
                code: 4,
                text: "a".into()
            }),
            [0, 0, 0, 4, 11, 4, 1, 97]
        );
        assert_eq!(enc(&Control::Lease(vec![1, 2, 200])), [0, 0, 0, 5, 12, 3, 1, 2, 200]);
    }

    #[test]
    fn lease_size_is_capped_on_both_sides() {
        assert!(encode_frame(&Control::Lease(vec![])).is_err());
        assert!(encode_frame(&Control::Lease(vec![0; MAX_LEASE_LEN + 1])).is_err());
        // A hand-built frame that claims an over-long lease is refused on decode.
        let mut payload = vec![12];
        payload.extend([0x81, 0x08]); // varint 1025
        payload.extend(vec![0; MAX_LEASE_LEN + 1]);
        let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
        frame.extend(payload);
        assert!(matches!(decode_frame::<Control>(&frame), Err(FrameError::Malformed(_))));
        assert!(matches!(
            decode_frame::<Control>(&[0, 0, 0, 2, 12, 0]),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn hello_encoding_ends_with_the_feature_bits() {
        let hello = |features| Control::Hello {
            version: PROTOCOL_VERSION,
            peer: PEER,
            name: "a".into(),
            os: Os::Linux,
            width: 1,
            height: 2,
            features,
        };
        let mut expected = vec![0, 0, 0, 24, 0, 1];
        expected.extend([7; 16]);
        expected.extend([1, 97, 2, 1, 2, 1]); // name "a", Linux, 1x2, features 1
        let frame = encode_frame(&hello(FEATURE_KEY_TEXT)).unwrap();
        assert_eq!(frame, expected);
        assert_eq!(frame.last(), Some(&1), "features is the last field");
        assert_eq!(encode_frame(&hello(0)).unwrap().last(), Some(&0));
        // Unknown bits survive the wire; whoever reads them ignores what it does not know.
        let all = encode_frame(&hello(u64::MAX)).unwrap();
        assert!(matches!(
            decode_frame::<Control>(&all),
            Ok(Some((Control::Hello { features: u64::MAX, .. }, _)))
        ));
    }

    #[test]
    fn key_text_rejects_invisible_and_bidi_trickery() {
        for bad in [
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{202E}',
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}',
            '\u{2028}',
            '\u{2029}',
            '\u{FEFF}',
            '\u{200B}',
            '\u{E0000}',
            '\u{E0041}',
            '\u{E007F}',
            '\u{FDD0}',
            '\u{FDEF}',
            '\u{FFFE}',
            '\u{FFFF}',
            '\u{1FFFE}',
            '\u{10FFFF}',
            '\u{10FFFE}',
        ] {
            assert!(!key_text_is_valid(&bad.to_string()), "{:X}", bad as u32);
            assert!(
                !key_text_is_valid(&format!("a{bad}")),
                "{:X} after a letter",
                bad as u32
            );
        }
        for good in [
            '\u{200D}',
            '\u{200C}',
            '\u{FE00}',
            '\u{FE0F}',
            '\u{E0100}',
            '\u{E01EF}',
            '\u{FDCF}',
            '\u{FDF0}',
        ] {
            assert!(key_text_is_valid(&format!("a{good}")), "{:X}", good as u32);
        }
    }

    #[test]
    fn key_text_rejects_marks_and_format_characters_that_hide_or_reorder() {
        for u in [
            0x200E, 0x200F, 0x061C, 0x2060, 0x2061, 0x2062, 0x2063, 0x2064, 0xAD, 0x180E, 0xFFF9, 0xFFFA, 0xFFFB,
        ] {
            let c = char::from_u32(u).unwrap();
            assert!(!key_text_is_valid(&format!("a{c}")), "{u:X}");
        }
    }

    #[test]
    fn hidden_set_covers_tags_noncharacters_and_fillers_and_matches_key_text() {
        for u in [
            0xE0000, 0xE0041, 0xE007F, 0xFDD0, 0xFDEF, 0xFFFE, 0x1FFFF, 0x10FFFF, 0x34F, 0x115F, 0x1160, 0x3164,
            0xFFA0, 0x17B4, 0x17B5,
        ] {
            let c = char::from_u32(u).unwrap();
            assert!(is_invisible(c), "{u:X}");
            assert!(!key_text_is_valid(&format!("a{c}")), "{u:X}");
        }
        // The joiners differ on purpose: allowed in key text, rejected in names.
        for c in ['\u{200C}', '\u{200D}'] {
            assert!(is_invisible(c) && key_text_is_valid(&format!("a{c}")));
        }
        assert!(!is_invisible('\u{FE0F}') && !is_invisible('a'));
    }

    #[test]
    fn clip_names_reject_windows_hazards() {
        for bad in [
            "a:b",
            "a<b",
            "a>b",
            "a\"b",
            "a|b",
            "a?b",
            "a*b",
            "C:",
            "name.",
            "name ",
            "CON",
            "con",
            "Nul.txt",
            "aux.tar.gz",
            "COM1",
            "lpt9.txt",
            "COM0",
            "prn ",
            "CON .txt",
            "CONIN$",
            "conout$.txt",
            "CLOCK$",
            "COM\u{B9}",
            "lpt\u{B2}.log",
            ".",
            "a\u{200D}b",
            "a\u{3164}",
            "\u{E0041}x",
        ] {
            assert!(!clip_name_is_valid(bad), "{bad:?}");
        }
        for good in ["console.txt", "COM10", "comma", "LPT", "a b.txt", "conx"] {
            assert!(clip_name_is_valid(good), "{good:?}");
        }
    }

    #[test]
    fn clip_names_must_be_plain_file_names() {
        for good in ["a.txt", "photo (1).png", "日本語.md", ".hidden", "a.b", "a..b"] {
            assert!(clip_name_is_valid(good), "{good:?}");
        }
        let long = "x".repeat(MAX_CLIP_NAME_BYTES + 1);
        for bad in [
            "",
            "..",
            "a/b",
            "a\\b",
            "../x",
            "a\0b",
            "a\nb",
            "a\u{202E}b",
            "a\u{200B}",
            "\u{FEFF}a",
            &long,
        ] {
            assert!(!clip_name_is_valid(bad), "{bad:?}");
        }
        assert!(clip_name_is_valid(&"x".repeat(MAX_CLIP_NAME_BYTES)));
    }

    #[test]
    fn key_text_is_bounded_and_printable() {
        assert!(key_text_is_valid("a") && key_text_is_valid("e\u{301}") && key_text_is_valid("€"));
        assert!(!key_text_is_valid(""));
        assert!(!key_text_is_valid("\n") && !key_text_is_valid("a\u{7f}"));
        assert!(key_text_is_valid(&"x".repeat(MAX_KEY_TEXT_CHARS)));
        assert!(!key_text_is_valid(&"x".repeat(MAX_KEY_TEXT_CHARS + 1)));
    }

    #[test]
    fn partial_frame_needs_more_bytes() {
        let frame = encode_frame(&Control::Key { code: 4, pressed: true }).unwrap();
        for cut in 0..frame.len() {
            assert_eq!(decode_frame::<Control>(&frame[..cut]), Ok(None), "cut at {cut}");
        }
    }

    #[test]
    fn back_to_back_frames_decode_in_order() {
        let a = Control::Key {
            code: 0xE3,
            pressed: true,
        };
        let b = Control::Key {
            code: 0xE3,
            pressed: false,
        };
        let mut buf = encode_frame(&a).unwrap();
        buf.extend(encode_frame(&b).unwrap());
        let (first, used): (Control, usize) = decode_frame(&buf).unwrap().unwrap();
        assert_eq!(first, a);
        assert_eq!(decode_frame::<Control>(&buf[used..]).unwrap().unwrap().0, b);
    }

    #[test]
    fn oversized_length_rejected_without_payload() {
        let len = (MAX_FRAME_LEN as u32 + 1).to_be_bytes();
        assert_eq!(
            decode_frame::<Control>(&len),
            Err(FrameError::TooLarge(MAX_FRAME_LEN + 1))
        );
    }

    #[test]
    fn oversized_message_refuses_to_encode() {
        let msg = Control::Clipboard {
            text: "x".repeat(2 * MAX_FRAME_LEN),
        };
        assert!(matches!(encode_frame(&msg), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn garbage_payload_is_malformed() {
        assert!(matches!(
            decode_frame::<Control>(&[0, 0, 0, 1, 0xFF]),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn trailing_bytes_inside_frame_are_malformed() {
        let mut frame = encode_frame(&Control::Key { code: 4, pressed: true }).unwrap();
        frame.push(0);
        let len = (frame.len() - LEN_PREFIX) as u32;
        frame[..LEN_PREFIX].copy_from_slice(&len.to_be_bytes());
        assert!(matches!(decode_frame::<Control>(&frame), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn motion_round_trips_and_fits_one_small_datagram() {
        let m = Motion {
            seq: u32::MAX,
            x: -1,
            y: 4319,
        };
        let d = encode_motion(&m);
        assert!(d.len() <= 16, "motion datagram is {} bytes", d.len());
        assert_eq!(decode_motion(&d), Ok(m));
    }

    #[test]
    fn motion_with_trailing_or_missing_bytes_is_malformed() {
        let mut d = encode_motion(&Motion { seq: 1, x: 2, y: 3 });
        assert!(matches!(
            decode_motion(&d[..d.len() - 1]),
            Err(FrameError::Malformed(_))
        ));
        d.push(0);
        assert!(matches!(decode_motion(&d), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn seq_gate_drops_stale_and_duplicate() {
        let mut g = SeqGate::default();
        assert!(g.accept(10));
        assert!(!g.accept(10));
        assert!(!g.accept(9));
        assert!(g.accept(12));
        assert!(!g.accept(11));
    }

    #[test]
    fn seq_gate_survives_wraparound() {
        let mut g = SeqGate::default();
        assert!(g.accept(u32::MAX - 1));
        assert!(g.accept(u32::MAX));
        assert!(g.accept(0));
        assert!(g.accept(1));
        assert!(!g.accept(u32::MAX));
    }

    #[test]
    fn pairing_messages_round_trip() {
        for msg in [
            Pairing::Commit { digest: [1; 32] },
            Pairing::Nonce { nonce: [2; 32] },
            Pairing::Reveal { nonce: [3; 32] },
            Pairing::Confirm,
            Pairing::Reject,
        ] {
            let frame = encode_frame(&msg).unwrap();
            assert_eq!(decode_frame::<Pairing>(&frame), Ok(Some((msg, frame.len()))));
        }
    }

    #[test]
    fn seq_gate_new_focus_period_drops_stale_motion() {
        let mut g = SeqGate::default();
        assert!(g.accept(100));
        g.start_at(200);
        assert!(!g.accept(150), "datagram from the previous focus period");
        assert!(g.accept(200));
        assert!(g.accept(201));
    }

    #[test]
    fn seq_gate_start_at_works_on_fresh_gate_and_across_wrap() {
        let mut g = SeqGate::default();
        g.start_at(0);
        assert!(!g.accept(u32::MAX));
        assert!(g.accept(0));
    }
}
