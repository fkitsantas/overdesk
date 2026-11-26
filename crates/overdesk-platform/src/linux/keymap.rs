// SPDX-License-Identifier: AGPL-3.0-only
//! X11 keycodes to and from Overdesk key codes (`page | HID usage`). An X11 keycode is the
//! Linux evdev code plus 8 on every modern server, so the table is keyed by evdev code:
//! layout-independent, like the scan codes of the Windows adapter.

use overdesk_proto::PAGE_CONSUMER;

/// X11 keycodes are evdev codes shifted up by 8 (the core protocol reserves 0..=7).
const EVDEV_OFFSET: u32 = 8;

/// `(evdev code, overdesk code)`, bijective.
const KEYS: &[(u16, u32)] = &[
    (1, 0x29),                   // Escape
    (2, 0x1E),                   // 1
    (3, 0x1F),                   // 2
    (4, 0x20),                   // 3
    (5, 0x21),                   // 4
    (6, 0x22),                   // 5
    (7, 0x23),                   // 6
    (8, 0x24),                   // 7
    (9, 0x25),                   // 8
    (10, 0x26),                  // 9
    (11, 0x27),                  // 0
    (12, 0x2D),                  // Minus
    (13, 0x2E),                  // Equal
    (14, 0x2A),                  // Backspace
    (15, 0x2B),                  // Tab
    (16, 0x14),                  // Q
    (17, 0x1A),                  // W
    (18, 0x08),                  // E
    (19, 0x15),                  // R
    (20, 0x17),                  // T
    (21, 0x1C),                  // Y
    (22, 0x18),                  // U
    (23, 0x0C),                  // I
    (24, 0x12),                  // O
    (25, 0x13),                  // P
    (26, 0x2F),                  // Left bracket
    (27, 0x30),                  // Right bracket
    (28, 0x28),                  // Enter
    (29, 0xE0),                  // Left Ctrl
    (30, 0x04),                  // A
    (31, 0x16),                  // S
    (32, 0x07),                  // D
    (33, 0x09),                  // F
    (34, 0x0A),                  // G
    (35, 0x0B),                  // H
    (36, 0x0D),                  // J
    (37, 0x0E),                  // K
    (38, 0x0F),                  // L
    (39, 0x33),                  // Semicolon
    (40, 0x34),                  // Quote
    (41, 0x35),                  // Grave
    (42, 0xE1),                  // Left Shift
    (43, 0x31),                  // Backslash
    (44, 0x1D),                  // Z
    (45, 0x1B),                  // X
    (46, 0x06),                  // C
    (47, 0x19),                  // V
    (48, 0x05),                  // B
    (49, 0x11),                  // N
    (50, 0x10),                  // M
    (51, 0x36),                  // Comma
    (52, 0x37),                  // Period
    (53, 0x38),                  // Slash
    (54, 0xE5),                  // Right Shift
    (55, 0x55),                  // Keypad *
    (56, 0xE2),                  // Left Alt
    (57, 0x2C),                  // Space
    (58, 0x39),                  // Caps Lock
    (59, 0x3A),                  // F1
    (60, 0x3B),                  // F2
    (61, 0x3C),                  // F3
    (62, 0x3D),                  // F4
    (63, 0x3E),                  // F5
    (64, 0x3F),                  // F6
    (65, 0x40),                  // F7
    (66, 0x41),                  // F8
    (67, 0x42),                  // F9
    (68, 0x43),                  // F10
    (69, 0x53),                  // Num Lock
    (70, 0x47),                  // Scroll Lock
    (71, 0x5F),                  // Keypad 7
    (72, 0x60),                  // Keypad 8
    (73, 0x61),                  // Keypad 9
    (74, 0x56),                  // Keypad -
    (75, 0x5C),                  // Keypad 4
    (76, 0x5D),                  // Keypad 5
    (77, 0x5E),                  // Keypad 6
    (78, 0x57),                  // Keypad +
    (79, 0x59),                  // Keypad 1
    (80, 0x5A),                  // Keypad 2
    (81, 0x5B),                  // Keypad 3
    (82, 0x62),                  // Keypad 0
    (83, 0x63),                  // Keypad .
    (86, 0x64),                  // ISO extra key
    (87, 0x44),                  // F11
    (88, 0x45),                  // F12
    (89, 0x87),                  // JIS ro
    (90, 0x88),                  // JIS kana
    (96, 0x58),                  // Keypad Enter
    (97, 0xE4),                  // Right Ctrl
    (98, 0x54),                  // Keypad /
    (99, 0x46),                  // Print Screen
    (100, 0xE6),                 // Right Alt / AltGr
    (102, 0x4A),                 // Home
    (103, 0x52),                 // Up
    (104, 0x4B),                 // Page Up
    (105, 0x50),                 // Left
    (106, 0x4F),                 // Right
    (107, 0x4D),                 // End
    (108, 0x51),                 // Down
    (109, 0x4E),                 // Page Down
    (110, 0x49),                 // Insert
    (111, 0x4C),                 // Delete
    (113, PAGE_CONSUMER | 0xE2), // Mute
    (114, PAGE_CONSUMER | 0xEA), // Volume down
    (115, PAGE_CONSUMER | 0xE9), // Volume up
    (117, 0x67),                 // Keypad =
    (119, 0x48),                 // Pause
    (121, 0x85),                 // Keypad comma
    (124, 0x89),                 // JIS yen
    (125, 0xE3),                 // Left Super
    (126, 0xE7),                 // Right Super
    (127, 0x65),                 // Application (menu)
    (163, PAGE_CONSUMER | 0xB5), // Next track
    (164, PAGE_CONSUMER | 0xCD), // Play / Pause
    (165, PAGE_CONSUMER | 0xB6), // Previous track
    (166, PAGE_CONSUMER | 0xB7), // Stop
    (183, 0x68),                 // F13
    (184, 0x69),                 // F14
    (185, 0x6A),                 // F15
    (186, 0x6B),                 // F16
    (187, 0x6C),                 // F17
    (188, 0x6D),                 // F18
    (189, 0x6E),                 // F19
    (190, 0x6F),                 // F20
    (191, 0x70),                 // F21
    (192, 0x71),                 // F22
    (193, 0x72),                 // F23
    (194, 0x73),                 // F24
];

/// One-way, for injection only: keyboard-page media keys a Mac sends that have a
/// consumer-page twin here (captured as consumer page).
const INJECT_ALIASES: &[(u32, u16)] = &[
    (0x7F, 113), // Mute
    (0x80, 115), // Volume up
    (0x81, 114), // Volume down
];

/// Translates an X11 keycode to its Overdesk code, or `None` for keys Overdesk does not map.
pub fn keycode_to_code(keycode: u32) -> Option<u32> {
    let evdev = keycode.checked_sub(EVDEV_OFFSET)?;
    KEYS.iter().find(|k| u32::from(k.0) == evdev).map(|k| k.1)
}

/// Translates an Overdesk code to the X11 keycode to inject, or `None` when Linux has no
/// such key. Falls back to `INJECT_ALIASES` so Mac keyboard-page media keys still work.
pub fn code_to_keycode(code: u32) -> Option<u8> {
    let evdev = KEYS
        .iter()
        .find(|k| k.1 == code)
        .map(|k| k.0)
        .or_else(|| INJECT_ALIASES.iter().find(|a| a.0 == code).map(|a| a.1))?;
    u8::try_from(u32::from(evdev) + EVDEV_OFFSET).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_is_bijective() {
        let evdevs: HashSet<_> = KEYS.iter().map(|k| k.0).collect();
        let codes: HashSet<_> = KEYS.iter().map(|k| k.1).collect();
        assert_eq!(evdevs.len(), KEYS.len(), "duplicate evdev code");
        assert_eq!(codes.len(), KEYS.len(), "duplicate code");
        for &(evdev, code) in KEYS {
            let keycode = u32::from(evdev) + EVDEV_OFFSET;
            assert_eq!(keycode_to_code(keycode), Some(code));
            assert_eq!(code_to_keycode(code), Some(keycode as u8));
        }
    }

    #[test]
    fn alphabet_digits_and_function_row_covered() {
        for usage in (0x04..=0x27).chain(0x3A..=0x45) {
            assert!(code_to_keycode(usage).is_some(), "HID usage {usage:#x} has no keycode");
        }
    }

    #[test]
    fn well_known_keycodes() {
        assert_eq!(keycode_to_code(9), Some(0x29)); // Escape
        assert_eq!(keycode_to_code(38), Some(0x04)); // A
        assert_eq!(keycode_to_code(50), Some(0xE1)); // left Shift
        assert_eq!(keycode_to_code(62), Some(0xE5)); // right Shift
        assert_eq!(keycode_to_code(133), Some(0xE3)); // left Super
        assert_eq!(code_to_keycode(0x2C), Some(65)); // Space
    }

    #[test]
    fn media_keys_use_consumer_page_with_mac_aliases() {
        assert_eq!(keycode_to_code(121), Some(PAGE_CONSUMER | 0xE2)); // mute
        assert_eq!(code_to_keycode(0x80), Some(123), "Mac keyboard-page volume up");
    }

    #[test]
    fn unmapped_keys_are_none() {
        assert_eq!(
            code_to_keycode(overdesk_proto::PAGE_APPLE_TOP_CASE | 0x03),
            None,
            "Fn/Globe"
        );
        assert_eq!(keycode_to_code(3), None, "below the evdev range");
        assert_eq!(keycode_to_code(255), None);
    }
}
