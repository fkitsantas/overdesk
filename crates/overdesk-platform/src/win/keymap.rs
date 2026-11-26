// SPDX-License-Identifier: AGPL-3.0-only
//! Windows scan codes (Set 1) to and from Overdesk key codes (`page | HID usage`).
//! Scan codes, not virtual keys: they are layout-independent and work in games.
//! A scan code is `0xE0nn` for extended keys, `0x00nn` otherwise.

use overdesk_proto::PAGE_CONSUMER;

/// `(scan code, overdesk code)`, bijective.
const KEYS: &[(u16, u32)] = &[];

/// One-way, for injection only: keyboard-page media keys a Mac sends that share a
/// Windows scan code with their consumer-page twin (captured as consumer page).
const INJECT_ALIASES: &[(u32, u16)] = &[];

pub fn scan_to_code(_scan: u16) -> Option<u32> {
    todo!()
}

pub fn code_to_scan(_code: u32) -> Option<u16> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_is_bijective() {
        let scans: HashSet<_> = KEYS.iter().map(|k| k.0).collect();
        let codes: HashSet<_> = KEYS.iter().map(|k| k.1).collect();
        assert_eq!(scans.len(), KEYS.len(), "duplicate scan code");
        assert_eq!(codes.len(), KEYS.len(), "duplicate code");
        for &(scan, code) in KEYS {
            assert_eq!(scan_to_code(scan), Some(code));
            assert_eq!(code_to_scan(code), Some(scan));
        }
    }

    #[test]
    fn alphabet_digits_and_function_row_covered() {
        for usage in (0x04..=0x27).chain(0x3A..=0x45) {
            assert!(code_to_scan(usage).is_some(), "HID usage {usage:#x} has no scan code");
        }
    }

    #[test]
    fn modifiers_keep_their_side() {
        assert_eq!(scan_to_code(0x001D), Some(0xE0)); // left Ctrl
        assert_eq!(scan_to_code(0xE01D), Some(0xE4)); // right Ctrl (extended)
        assert_eq!(scan_to_code(0xE05B), Some(0xE3)); // left Windows
        assert_eq!(scan_to_code(0xE038), Some(0xE6)); // right Alt / AltGr
        assert_eq!(scan_to_code(0x0036), Some(0xE5)); // right Shift
    }

    #[test]
    fn navigation_keys_are_extended() {
        assert_eq!(code_to_scan(0x49), Some(0xE052)); // Insert
        assert_eq!(code_to_scan(0x4C), Some(0xE053)); // Delete
        assert_eq!(code_to_scan(0x52), Some(0xE048)); // Up arrow
        assert_eq!(code_to_scan(0x60), Some(0x0048)); // keypad 8 shares the base code, not extended
    }

    #[test]
    fn media_keys_use_consumer_page_with_mac_aliases() {
        assert_eq!(scan_to_code(0xE022), Some(PAGE_CONSUMER | 0xCD)); // play/pause
        assert_eq!(scan_to_code(0xE030), Some(PAGE_CONSUMER | 0xE9)); // volume up
        assert_eq!(code_to_scan(0x80), Some(0xE030), "Mac keyboard-page volume up");
        assert_eq!(code_to_scan(0x7F), Some(0xE020), "Mac keyboard-page mute");
    }

    #[test]
    fn keys_windows_cannot_type_are_none() {
        assert_eq!(code_to_scan(overdesk_proto::PAGE_APPLE_TOP_CASE | 0x03), None, "Fn/Globe");
        assert_eq!(scan_to_code(0xE0FF), None);
    }
}
