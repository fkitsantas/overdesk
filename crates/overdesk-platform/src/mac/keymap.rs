// SPDX-License-Identifier: AGPL-3.0-only
//! macOS virtual keycodes (`kVK_*`, HIToolbox Events.h) and NX system-defined media keys
//! to and from Overdesk key codes (`page | HID usage`).

use overdesk_proto::{PAGE_APPLE_TOP_CASE, PAGE_CONSUMER};

/// `(kVK, overdesk code)`.
const KEYS: &[(u16, u32)] = &[
    (0x00, 0x04),                       // A
    (0x0B, 0x05),                       // B
    (0x08, 0x06),                       // C
    (0x02, 0x07),                       // D
    (0x0E, 0x08),                       // E
    (0x03, 0x09),                       // F
    (0x05, 0x0A),                       // G
    (0x04, 0x0B),                       // H
    (0x22, 0x0C),                       // I
    (0x26, 0x0D),                       // J
    (0x28, 0x0E),                       // K
    (0x25, 0x0F),                       // L
    (0x2E, 0x10),                       // M
    (0x2D, 0x11),                       // N
    (0x1F, 0x12),                       // O
    (0x23, 0x13),                       // P
    (0x0C, 0x14),                       // Q
    (0x0F, 0x15),                       // R
    (0x01, 0x16),                       // S
    (0x11, 0x17),                       // T
    (0x20, 0x18),                       // U
    (0x09, 0x19),                       // V
    (0x0D, 0x1A),                       // W
    (0x07, 0x1B),                       // X
    (0x10, 0x1C),                       // Y
    (0x06, 0x1D),                       // Z
    (0x12, 0x1E),                       // 1
    (0x13, 0x1F),                       // 2
    (0x14, 0x20),                       // 3
    (0x15, 0x21),                       // 4
    (0x17, 0x22),                       // 5
    (0x16, 0x23),                       // 6
    (0x1A, 0x24),                       // 7
    (0x1C, 0x25),                       // 8
    (0x19, 0x26),                       // 9
    (0x1D, 0x27),                       // 0
    (0x24, 0x28),                       // Return
    (0x35, 0x29),                       // Escape
    (0x33, 0x2A),                       // Delete (Backspace)
    (0x30, 0x2B),                       // Tab
    (0x31, 0x2C),                       // Space
    (0x1B, 0x2D),                       // Minus
    (0x18, 0x2E),                       // Equal
    (0x21, 0x2F),                       // Left bracket
    (0x1E, 0x30),                       // Right bracket
    (0x2A, 0x31),                       // Backslash
    (0x29, 0x33),                       // Semicolon
    (0x27, 0x34),                       // Quote
    (0x32, 0x35),                       // Grave
    (0x2B, 0x36),                       // Comma
    (0x2F, 0x37),                       // Period
    (0x2C, 0x38),                       // Slash
    (0x39, 0x39),                       // Caps Lock
    (0x7A, 0x3A),                       // F1
    (0x78, 0x3B),                       // F2
    (0x63, 0x3C),                       // F3
    (0x76, 0x3D),                       // F4
    (0x60, 0x3E),                       // F5
    (0x61, 0x3F),                       // F6
    (0x62, 0x40),                       // F7
    (0x64, 0x41),                       // F8
    (0x65, 0x42),                       // F9
    (0x6D, 0x43),                       // F10
    (0x67, 0x44),                       // F11
    (0x6F, 0x45),                       // F12
    (0x72, 0x49),                       // Help <-> PC Insert
    (0x73, 0x4A),                       // Home
    (0x74, 0x4B),                       // Page Up
    (0x75, 0x4C),                       // Forward Delete
    (0x77, 0x4D),                       // End
    (0x79, 0x4E),                       // Page Down
    (0x7C, 0x4F),                       // Right
    (0x7B, 0x50),                       // Left
    (0x7D, 0x51),                       // Down
    (0x7E, 0x52),                       // Up
    (0x47, 0x53),                       // Keypad Clear <-> Num Lock
    (0x4B, 0x54),                       // Keypad /
    (0x43, 0x55),                       // Keypad *
    (0x4E, 0x56),                       // Keypad -
    (0x45, 0x57),                       // Keypad +
    (0x4C, 0x58),                       // Keypad Enter
    (0x53, 0x59),                       // Keypad 1
    (0x54, 0x5A),                       // Keypad 2
    (0x55, 0x5B),                       // Keypad 3
    (0x56, 0x5C),                       // Keypad 4
    (0x57, 0x5D),                       // Keypad 5
    (0x58, 0x5E),                       // Keypad 6
    (0x59, 0x5F),                       // Keypad 7
    (0x5B, 0x60),                       // Keypad 8
    (0x5C, 0x61),                       // Keypad 9
    (0x52, 0x62),                       // Keypad 0
    (0x41, 0x63),                       // Keypad .
    (0x0A, 0x64),                       // ISO section
    (0x6E, 0x65),                       // PC Application (context menu)
    (0x51, 0x67),                       // Keypad =
    (0x69, 0x68),                       // F13
    (0x6B, 0x69),                       // F14
    (0x71, 0x6A),                       // F15
    (0x6A, 0x6B),                       // F16
    (0x40, 0x6C),                       // F17
    (0x4F, 0x6D),                       // F18
    (0x50, 0x6E),                       // F19
    (0x5A, 0x6F),                       // F20
    (0x4A, 0x7F),                       // Mute
    (0x48, 0x80),                       // Volume Up
    (0x49, 0x81),                       // Volume Down
    (0x5F, 0x85),                       // JIS keypad comma
    (0x5E, 0x87),                       // JIS underscore (International1)
    (0x5D, 0x89),                       // JIS yen (International3)
    (0x68, 0x90),                       // JIS kana (LANG1)
    (0x66, 0x91),                       // JIS eisu (LANG2)
    (0x3B, 0xE0),                       // Control
    (0x38, 0xE1),                       // Shift
    (0x3A, 0xE2),                       // Option
    (0x37, 0xE3),                       // Command
    (0x3E, 0xE4),                       // Right Control
    (0x3C, 0xE5),                       // Right Shift
    (0x3D, 0xE6),                       // Right Option
    (0x36, 0xE7),                       // Right Command
    (0x3F, PAGE_APPLE_TOP_CASE | 0x03), // Fn / Globe
];

/// `(NX_KEYTYPE_*, overdesk code)`: media keys arrive as system-defined events, not key events.
const MEDIA: &[(u32, u32)] = &[
    (0, PAGE_CONSUMER | 0xE9),  // Volume Up
    (1, PAGE_CONSUMER | 0xEA),  // Volume Down
    (2, PAGE_CONSUMER | 0x6F),  // Brightness Up
    (3, PAGE_CONSUMER | 0x70),  // Brightness Down
    (7, PAGE_CONSUMER | 0xE2),  // Mute
    (14, PAGE_CONSUMER | 0xB8), // Eject
    (16, PAGE_CONSUMER | 0xCD), // Play / Pause
    (17, PAGE_CONSUMER | 0xB5), // Next track
    (18, PAGE_CONSUMER | 0xB6), // Previous track
    (19, PAGE_CONSUMER | 0xB3), // Fast forward
    (20, PAGE_CONSUMER | 0xB4), // Rewind
];

// Linear scans: ~120 entries, called once per key event; cheaper than building a map.
pub fn vk_to_code(vk: u16) -> Option<u32> {
    KEYS.iter().find(|k| k.0 == vk).map(|k| k.1)
}

pub fn code_to_vk(code: u32) -> Option<u16> {
    KEYS.iter().find(|k| k.1 == code).map(|k| k.0)
}

pub fn nx_to_code(nx: u32) -> Option<u32> {
    MEDIA.iter().find(|k| k.0 == nx).map(|k| k.1)
}

pub fn code_to_nx(code: u32) -> Option<u32> {
    MEDIA.iter().find(|k| k.1 == code).map(|k| k.0)
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
