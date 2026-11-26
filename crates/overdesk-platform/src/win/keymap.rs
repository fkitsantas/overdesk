// SPDX-License-Identifier: AGPL-3.0-only
//! Windows scan codes (Set 1) to and from Overdesk key codes (`page | HID usage`).
//! Scan codes, not virtual keys: they are layout-independent and work in games.
//! A scan code is `0xE0nn` for extended keys, `0x00nn` otherwise.

use overdesk_proto::PAGE_CONSUMER;

/// `(scan code, overdesk code)`, bijective.
const KEYS: &[(u16, u32)] = &[
    (0x001E, 0x04),                 // A
    (0x0030, 0x05),                 // B
    (0x002E, 0x06),                 // C
    (0x0020, 0x07),                 // D
    (0x0012, 0x08),                 // E
    (0x0021, 0x09),                 // F
    (0x0022, 0x0A),                 // G
    (0x0023, 0x0B),                 // H
    (0x0017, 0x0C),                 // I
    (0x0024, 0x0D),                 // J
    (0x0025, 0x0E),                 // K
    (0x0026, 0x0F),                 // L
    (0x0032, 0x10),                 // M
    (0x0031, 0x11),                 // N
    (0x0018, 0x12),                 // O
    (0x0019, 0x13),                 // P
    (0x0010, 0x14),                 // Q
    (0x0013, 0x15),                 // R
    (0x001F, 0x16),                 // S
    (0x0014, 0x17),                 // T
    (0x0016, 0x18),                 // U
    (0x002F, 0x19),                 // V
    (0x0011, 0x1A),                 // W
    (0x002D, 0x1B),                 // X
    (0x0015, 0x1C),                 // Y
    (0x002C, 0x1D),                 // Z
    (0x0002, 0x1E),                 // 1
    (0x0003, 0x1F),                 // 2
    (0x0004, 0x20),                 // 3
    (0x0005, 0x21),                 // 4
    (0x0006, 0x22),                 // 5
    (0x0007, 0x23),                 // 6
    (0x0008, 0x24),                 // 7
    (0x0009, 0x25),                 // 8
    (0x000A, 0x26),                 // 9
    (0x000B, 0x27),                 // 0
    (0x001C, 0x28),                 // Enter
    (0x0001, 0x29),                 // Escape
    (0x000E, 0x2A),                 // Backspace
    (0x000F, 0x2B),                 // Tab
    (0x0039, 0x2C),                 // Space
    (0x000C, 0x2D),                 // Minus
    (0x000D, 0x2E),                 // Equal
    (0x001A, 0x2F),                 // Left bracket
    (0x001B, 0x30),                 // Right bracket
    (0x002B, 0x31),                 // Backslash
    (0x0027, 0x33),                 // Semicolon
    (0x0028, 0x34),                 // Quote
    (0x0029, 0x35),                 // Grave
    (0x0033, 0x36),                 // Comma
    (0x0034, 0x37),                 // Period
    (0x0035, 0x38),                 // Slash
    (0x003A, 0x39),                 // Caps Lock
    (0x003B, 0x3A),                 // F1
    (0x003C, 0x3B),                 // F2
    (0x003D, 0x3C),                 // F3
    (0x003E, 0x3D),                 // F4
    (0x003F, 0x3E),                 // F5
    (0x0040, 0x3F),                 // F6
    (0x0041, 0x40),                 // F7
    (0x0042, 0x41),                 // F8
    (0x0043, 0x42),                 // F9
    (0x0044, 0x43),                 // F10
    (0x0057, 0x44),                 // F11
    (0x0058, 0x45),                 // F12
    (0xE037, 0x46),                 // Print Screen
    (0x0046, 0x47),                 // Scroll Lock
    (0xE052, 0x49),                 // Insert
    (0xE047, 0x4A),                 // Home
    (0xE049, 0x4B),                 // Page Up
    (0xE053, 0x4C),                 // Delete
    (0xE04F, 0x4D),                 // End
    (0xE051, 0x4E),                 // Page Down
    (0xE04D, 0x4F),                 // Right
    (0xE04B, 0x50),                 // Left
    (0xE050, 0x51),                 // Down
    (0xE048, 0x52),                 // Up
    (0x0045, 0x53),                 // Num Lock
    (0xE035, 0x54),                 // Keypad /
    (0x0037, 0x55),                 // Keypad *
    (0x004A, 0x56),                 // Keypad -
    (0x004E, 0x57),                 // Keypad +
    (0xE01C, 0x58),                 // Keypad Enter
    (0x004F, 0x59),                 // Keypad 1
    (0x0050, 0x5A),                 // Keypad 2
    (0x0051, 0x5B),                 // Keypad 3
    (0x004B, 0x5C),                 // Keypad 4
    (0x004C, 0x5D),                 // Keypad 5
    (0x004D, 0x5E),                 // Keypad 6
    (0x0047, 0x5F),                 // Keypad 7
    (0x0048, 0x60),                 // Keypad 8
    (0x0049, 0x61),                 // Keypad 9
    (0x0052, 0x62),                 // Keypad 0
    (0x0053, 0x63),                 // Keypad .
    (0x0056, 0x64),                 // ISO extra key (\\ |)
    (0xE05D, 0x65),                 // Application (menu)
    (0x0059, 0x67),                 // Keypad =
    (0x0064, 0x68),                 // F13
    (0x0065, 0x69),                 // F14
    (0x0066, 0x6A),                 // F15
    (0x0067, 0x6B),                 // F16
    (0x0068, 0x6C),                 // F17
    (0x0069, 0x6D),                 // F18
    (0x006A, 0x6E),                 // F19
    (0x006B, 0x6F),                 // F20
    (0x006C, 0x70),                 // F21
    (0x006D, 0x71),                 // F22
    (0x006E, 0x72),                 // F23
    (0x0076, 0x73),                 // F24
    (0x007E, 0x85),                 // Keypad comma (JIS)
    (0x0073, 0x87),                 // International1 (JIS ro)
    (0x0070, 0x88),                 // International2 (JIS kana)
    (0x007D, 0x89),                 // International3 (JIS yen)
    (0x001D, 0xE0),                 // Left Ctrl
    (0x002A, 0xE1),                 // Left Shift
    (0x0038, 0xE2),                 // Left Alt
    (0xE05B, 0xE3),                 // Left Windows
    (0xE01D, 0xE4),                 // Right Ctrl
    (0x0036, 0xE5),                 // Right Shift
    (0xE038, 0xE6),                 // Right Alt / AltGr
    (0xE05C, 0xE7),                 // Right Windows
    (0xE022, PAGE_CONSUMER | 0xCD), // Play / Pause
    (0xE024, PAGE_CONSUMER | 0xB7), // Stop
    (0xE019, PAGE_CONSUMER | 0xB5), // Next track
    (0xE010, PAGE_CONSUMER | 0xB6), // Previous track
    (0xE020, PAGE_CONSUMER | 0xE2), // Mute
    (0xE030, PAGE_CONSUMER | 0xE9), // Volume up
    (0xE02E, PAGE_CONSUMER | 0xEA), // Volume down
];

/// One-way, for injection only: keyboard-page media keys a Mac sends that share a
/// Windows scan code with their consumer-page twin (captured as consumer page).
const INJECT_ALIASES: &[(u32, u16)] = &[
    (0x7F, 0xE020), // Mute
    (0x80, 0xE030), // Volume up
    (0x81, 0xE02E), // Volume down
];

/// Translates a Set 1 scan code (with `0xE0` in the high byte for extended keys, as
/// reported by the `WH_KEYBOARD_LL` hook) to its Overdesk code, or `None` for keys
/// Overdesk does not map.
pub fn scan_to_code(scan: u16) -> Option<u32> {
    KEYS.iter().find(|k| k.0 == scan).map(|k| k.1)
}

/// Translates an Overdesk code to the Set 1 scan code `SendInput` injects for it, or
/// `None` when Windows has no such key. Falls back to `INJECT_ALIASES` so Mac
/// keyboard-page media keys still work.
pub fn code_to_scan(code: u32) -> Option<u16> {
    KEYS.iter()
        .find(|k| k.1 == code)
        .map(|k| k.0)
        .or_else(|| INJECT_ALIASES.iter().find(|a| a.0 == code).map(|a| a.1))
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
        assert_eq!(
            code_to_scan(overdesk_proto::PAGE_APPLE_TOP_CASE | 0x03),
            None,
            "Fn/Globe"
        );
        assert_eq!(scan_to_code(0xE0FF), None);
    }
}
