// SPDX-License-Identifier: AGPL-3.0-only
//! Overdesk wire protocol. See `docs/ARCHITECTURE.md`.
//!
//! - Control lane: reliable stream of length-prefixed [`Control`] frames.
//! - Motion lane: unreliable datagrams, one bare [`Motion`] each, newest wins via [`SeqGate`].

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const PROTOCOL_VERSION: u16 = 1;
/// Frames above this are rejected before allocating, so a peer cannot exhaust memory.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;
const LEN_PREFIX: usize = 4;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    MacOs,
    Windows,
    Linux,
    Android,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Units of a scroll delta. Apple Magic Mouse / trackpads report pixels; notched wheels lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollUnit {
    Pixel,
    /// 1/120 of a notch, matching Windows `WHEEL_DELTA` granularity.
    Notch120,
}

/// Phase of a continuous scroll or gesture; lets the receiving Mac reproduce
/// rubber-banding and momentum (inertial) scrolling. Gestures never use `Momentum*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    None,
    Began,
    Changed,
    Ended,
    Cancelled,
    /// Fingers touched the trackpad but have not moved yet; stops running momentum.
    MayBegin,
    MomentumBegan,
    Momentum,
    MomentumEnded,
}

/// Trackpad gestures, delivered natively on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Gesture {
    /// Pinch; scale delta in thousandths (`1000` = +100%).
    Magnify { milli: i32 },
    /// Rotation delta in thousandths of a degree.
    Rotate { millideg: i32 },
    /// Two-finger double tap.
    SmartZoom,
}

/// Reliable, ordered control-lane message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    Hello {
        version: u16,
        peer: PeerId,
        name: String,
        os: Os,
        width: u32,
        height: u32,
    },
    /// Cursor focus moves to `peer`, at that peer's local logical coordinates. Motion
    /// datagrams of this focus period start at `seq`; older ones are stale.
    Focus {
        peer: PeerId,
        x: i32,
        y: i32,
        seq: u32,
    },
    /// Carries the pointer position: the control lane is not ordered against motion
    /// datagrams, so a click must not depend on the last datagram having arrived.
    Button {
        button: Button,
        pressed: bool,
        x: i32,
        y: i32,
    },
    Scroll {
        dx: i32,
        dy: i32,
        unit: ScrollUnit,
        phase: Phase,
    },
    Gesture {
        gesture: Gesture,
        phase: Phase,
    },
    /// `code` is `page_selector | HID usage`; see [`PAGE_KEYBOARD`].
    Key {
        code: u32,
        pressed: bool,
    },
    /// Replicated Desk layout (ARCHITECTURE §3).
    Desk(Desk),
    /// Interim: plain text. Replaced by lazy offer/request clipboard (ARCHITECTURE §6).
    Clipboard {
        text: String,
    },
}

/// One screen in the shared logical plane, owned by `peer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskScreen {
    pub peer: PeerId,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// The replicated Desk document. Carries layout only, never trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desk {
    pub version: u64,
    pub author: PeerId,
    pub screens: Vec<DeskScreen>,
}

/// Pre-trust handshake (ARCHITECTURE §7). Its own type, so an unpaired peer cannot
/// send `Control` at all: those bytes never decode as pairing messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pairing {
    /// Dialer commits to its nonce before seeing the listener's.
    Commit {
        digest: [u8; 32],
    },
    Nonce {
        nonce: [u8; 32],
    },
    Reveal {
        nonce: [u8; 32],
    },
    /// The user confirmed that both screens show the same code.
    Confirm,
    Reject,
}

/// Motion-lane datagram: absolute pointer position on the focused screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Motion {
    pub seq: u32,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame length {0} exceeds limit {MAX_FRAME_LEN}")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(String),
}

fn malformed(e: postcard::Error) -> FrameError {
    FrameError::Malformed(e.to_string())
}

/// Encode one stream message as `u32 big-endian length || postcard payload`.
/// Fails with `TooLarge` rather than emit a frame the peer would reject.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let payload = postcard::to_stdvec(msg).map_err(malformed)?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut frame = Vec::with_capacity(LEN_PREFIX + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
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
    let (msg, rest) = postcard::take_from_bytes(payload).map_err(malformed)?;
    if !rest.is_empty() {
        return Err(FrameError::Malformed(format!("{} trailing bytes", rest.len())));
    }
    Ok(Some((msg, LEN_PREFIX + len)))
}

/// Encode a motion datagram (no length prefix: the datagram is the frame).
pub fn encode_motion(m: &Motion) -> Vec<u8> {
    // Three varints into a Vec cannot fail.
    postcard::to_stdvec(m).expect("postcard serialization of Motion is infallible")
}

pub fn decode_motion(datagram: &[u8]) -> Result<Motion, FrameError> {
    let (m, rest) = postcard::take_from_bytes(datagram).map_err(malformed)?;
    if !rest.is_empty() {
        return Err(FrameError::Malformed(format!("{} trailing bytes", rest.len())));
    }
    Ok(m)
}

/// Accepts only motion newer than the last accepted one, using wrap-safe
/// serial-number arithmetic so `seq` may overflow freely. Create one per connection:
/// a restarted sender reconnects, so it never meets a stale gate.
#[derive(Debug, Default)]
pub struct SeqGate {
    last: Option<u32>,
}

impl SeqGate {
    /// Starts a new focus period: only `seq` and later are accepted from now on.
    pub fn start_at(&mut self, seq: u32) {
        self.last = Some(seq.wrapping_sub(1));
    }

    pub fn accept(&mut self, seq: u32) -> bool {
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
