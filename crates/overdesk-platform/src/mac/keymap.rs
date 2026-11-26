// SPDX-License-Identifier: AGPL-3.0-only
//! macOS virtual keycodes (`kVK_*`, HIToolbox Events.h) and NX system-defined media keys
//! to and from Overdesk key codes (`page | HID usage`).

use overdesk_proto::{PAGE_APPLE_TOP_CASE, PAGE_CONSUMER};

/// `(kVK, overdesk code)`.
const KEYS: &[(u16, u32)] = &[];

/// `(NX_KEYTYPE_*, overdesk code)`: media keys arrive as system-defined events, not key events.
const MEDIA: &[(u32, u32)] = &[];

pub fn vk_to_code(_vk: u16) -> Option<u32> {
    todo!()
}

pub fn code_to_vk(_code: u32) -> Option<u16> {
    todo!()
}

pub fn nx_to_code(_nx: u32) -> Option<u32> {
    todo!()
}

pub fn code_to_nx(_code: u32) -> Option<u32> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn tables_are_bijective() {
        let vks: HashSet<_> = KEYS.iter().map(|k| k.0).collect();
        let codes: HashSet<_> = KEYS.iter().map(|k| k.1).collect();
        assert_eq!(vks.len(), KEYS.len(), "duplicate kVK");
        assert_eq!(codes.len(), KEYS.len(), "duplicate code");
        for &(vk, code) in KEYS {
            assert_eq!(vk_to_code(vk), Some(code));
            assert_eq!(code_to_vk(code), Some(vk));
        }
        for &(nx, code) in MEDIA {
            assert_eq!(nx_to_code(nx), Some(code));
            assert_eq!(code_to_nx(code), Some(nx));
        }
    }

    #[test]
    fn whole_alphabet_digits_and_function_row_covered() {
        for usage in (0x04..=0x27).chain(0x3A..=0x45) {
            assert!(code_to_vk(usage).is_some(), "HID usage {usage:#x} has no kVK");
        }
    }

    #[test]
    fn apple_specific_keys() {
        assert_eq!(vk_to_code(0x00), Some(0x04)); // kVK_ANSI_A -> A
        assert_eq!(vk_to_code(0x37), Some(0xE3)); // Command -> left GUI
        assert_eq!(vk_to_code(0x36), Some(0xE7)); // right Command -> right GUI
        assert_eq!(vk_to_code(0x3A), Some(0xE2)); // Option -> left Alt
        assert_eq!(vk_to_code(0x3F), Some(PAGE_APPLE_TOP_CASE | 0x03)); // Fn / Globe
        assert_eq!(vk_to_code(0x33), Some(0x2A)); // Delete -> Backspace
        assert_eq!(vk_to_code(0x75), Some(0x4C)); // Forward Delete
        assert_eq!(vk_to_code(0x0A), Some(0x64)); // ISO section key
    }

    #[test]
    fn pc_insert_lands_on_mac_help() {
        assert_eq!(code_to_vk(0x49), Some(0x72));
    }

    #[test]
    fn media_keys_use_consumer_page() {
        assert_eq!(nx_to_code(16), Some(PAGE_CONSUMER | 0xCD)); // play/pause
        assert_eq!(nx_to_code(0), Some(PAGE_CONSUMER | 0xE9)); // volume up
        assert_eq!(nx_to_code(14), Some(PAGE_CONSUMER | 0xB8)); // eject
        assert_eq!(code_to_nx(PAGE_CONSUMER | 0x6F), Some(2)); // brightness up
    }

    #[test]
    fn unknown_codes_are_none() {
        assert_eq!(vk_to_code(0xFFFF), None);
        assert_eq!(code_to_vk(PAGE_CONSUMER | 0x04), None);
        assert_eq!(nx_to_code(999), None);
    }
}
