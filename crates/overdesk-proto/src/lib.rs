// SPDX-License-Identifier: AGPL-3.0-only
//! Overdesk wire protocol: messages plus length-prefixed framing.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
/// Frames above this are rejected before allocating, so a peer cannot exhaust memory.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;
const LEN_PREFIX: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    MacOs,
    Windows,
    Linux,
    Android,
}

/// Units of a scroll delta. Apple Magic Mouse / trackpads report pixels; notched wheels lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollUnit {
    Pixel,
    /// 1/120 of a notch, matching Windows `WHEEL_DELTA` granularity.
    Notch120,
}

/// Gesture phase of continuous scrolling; required to reproduce macOS rubber-banding
/// and momentum (inertial) scrolling on the receiving Mac.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollPhase {
    None,
    Began,
    Changed,
    Ended,
    Cancelled,
    MomentumBegan,
    Momentum,
    MomentumEnded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello {
        version: u16,
        name: String,
        os: Os,
        width: u32,
        height: u32,
    },
    /// Cursor enters the receiving screen at local coordinates.
    Enter {
        x: i32,
        y: i32,
    },
    Leave,
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: Button,
        pressed: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
        unit: ScrollUnit,
        phase: ScrollPhase,
    },
    /// `code` is a platform-neutral USB HID usage id.
    Key {
        code: u32,
        pressed: bool,
    },
    Clipboard {
        text: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame length {0} exceeds limit {MAX_FRAME_LEN}")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(String),
}

/// Encode one message as `u32 big-endian length || postcard payload`.
/// Fails with `TooLarge` rather than emit a frame the peer would reject.
pub fn encode(msg: &Message) -> Result<Vec<u8>, FrameError> {
    let payload = postcard::to_stdvec(msg).map_err(|e| FrameError::Malformed(e.to_string()))?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut frame = Vec::with_capacity(LEN_PREFIX + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode one frame from the front of `buf`.
/// `Ok(None)` means more bytes are needed; `Ok(Some((msg, consumed)))` otherwise.
pub fn decode(buf: &[u8]) -> Result<Option<(Message, usize)>, FrameError> {
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
    let (msg, rest) = postcard::take_from_bytes(payload).map_err(|e| FrameError::Malformed(e.to_string()))?;
    if !rest.is_empty() {
        return Err(FrameError::Malformed(format!("{} trailing bytes", rest.len())));
    }
    Ok(Some((msg, LEN_PREFIX + len)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_messages() -> Vec<Message> {
        vec![
            Message::Hello {
                version: PROTOCOL_VERSION,
                name: "mac-mini".into(),
                os: Os::MacOs,
                width: 2560,
                height: 1440,
            },
            Message::Enter { x: 0, y: 540 },
            Message::Leave,
            Message::MouseMove { x: -5, y: 1079 },
            Message::MouseButton {
                button: Button::Left,
                pressed: true,
            },
            Message::Scroll {
                dx: 0,
                dy: -120,
                unit: ScrollUnit::Notch120,
                phase: ScrollPhase::None,
            },
            Message::Scroll {
                dx: -3,
                dy: 17,
                unit: ScrollUnit::Pixel,
                phase: ScrollPhase::Momentum,
            },
            Message::Key {
                code: 0x04,
                pressed: false,
            },
            Message::Clipboard {
                text: "héllo 🌍".into(),
            },
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for msg in all_messages() {
            let frame = encode(&msg).unwrap();
            assert_eq!(decode(&frame), Ok(Some((msg, frame.len()))));
        }
    }

    #[test]
    fn partial_frame_needs_more_bytes() {
        let frame = encode(&Message::Leave).unwrap();
        for cut in 0..frame.len() {
            assert_eq!(decode(&frame[..cut]), Ok(None), "cut at {cut}");
        }
    }

    #[test]
    fn back_to_back_frames_decode_in_order() {
        let mut buf = encode(&Message::Leave).unwrap();
        buf.extend(
            encode(&Message::Key {
                code: 0xE3,
                pressed: true,
            })
            .unwrap(),
        );
        let (first, used) = decode(&buf).unwrap().unwrap();
        assert_eq!(first, Message::Leave);
        let (second, _) = decode(&buf[used..]).unwrap().unwrap();
        assert_eq!(
            second,
            Message::Key {
                code: 0xE3,
                pressed: true
            }
        );
    }

    #[test]
    fn oversized_length_rejected_without_payload() {
        let len = (MAX_FRAME_LEN as u32 + 1).to_be_bytes();
        assert_eq!(decode(&len), Err(FrameError::TooLarge(MAX_FRAME_LEN + 1)));
    }

    #[test]
    fn oversized_message_refuses_to_encode() {
        let msg = Message::Clipboard {
            text: "x".repeat(2 * MAX_FRAME_LEN),
        };
        assert!(matches!(encode(&msg), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn garbage_payload_is_malformed() {
        let buf = [0, 0, 0, 1, 0xFF];
        assert!(matches!(decode(&buf), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn trailing_bytes_inside_frame_are_malformed() {
        let mut frame = encode(&Message::Leave).unwrap();
        frame.push(0);
        let len = (frame.len() - LEN_PREFIX) as u32;
        frame[..LEN_PREFIX].copy_from_slice(&len.to_be_bytes());
        assert!(matches!(decode(&frame), Err(FrameError::Malformed(_))));
    }
}
