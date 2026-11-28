// SPDX-License-Identifier: AGPL-3.0-only
//! Lower-case hex for identifiers (fingerprints, peer ids, keys): the one encoding every crate uses.

use std::fmt::Write;

/// Lower-case hex of `bytes`: two characters per byte.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(2 * bytes.len()), |mut s, b| {
        let _ = write!(s, "{b:02x}"); // writing to a String cannot fail
        s
    })
}

/// Inverse of [`hex`]. `None` for any length other than `2 * N` or a non-hex character
/// (so a malformed line never panics on a slice boundary).
pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    // `from_str_radix` would accept a leading '+', so every byte must be a hex digit.
    if s.len() != 2 * N || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Inverse of [`hex`] for any even length. `None` for an odd length or a non-hex character.
pub fn unhex_vec(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_id_round_trips() {
        let id = crate::PeerId([0xab; 16]);
        assert_eq!(crate::PeerId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(crate::PeerId::from_hex("ab"), None);
    }

    #[test]
    fn unhex_accepts_only_hex_digits() {
        assert_eq!(unhex::<2>("0aFf"), Some([0x0a, 0xff]));
        assert_eq!(unhex::<2>("+a+f"), None, "a sign is not a digit");
        assert_eq!(unhex::<2>("0aé"), None);
        assert_eq!(unhex::<2>("0a"), None);
        assert_eq!(unhex_vec("0aFf00"), Some(vec![0x0a, 0xff, 0]));
        assert_eq!(unhex_vec("0aF"), None);
        assert_eq!(unhex_vec("+a"), None);
        assert_eq!(unhex_vec("zz"), None);
        assert_eq!(unhex_vec("é"), None);
        assert_eq!(unhex_vec(""), Some(vec![]));
    }
}
