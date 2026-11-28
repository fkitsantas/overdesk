// SPDX-License-Identifier: AGPL-3.0-only
//! Choosing the keysym a key gives under the X server's keyboard mapping, and what it is.
//! Pure, so it is tested on every OS; the X11 requests live in `layout`.
//!
//! The core protocol's mapping lists, per keycode: group 1 levels 1-2, group 2 levels 1-2,
//! then group 1 levels 3 and up (the AltGr levels). `shortcut:` keys of more than two
//! groups, and levels above 4, are not read; upgrade by reading the XKB key types.

use xkeysym::Keysym;

use crate::text::OsMods;

/// The whole core keyboard mapping, fetched once and used until the server says it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMap {
    /// The keycode of the first entry.
    pub min: u8,
    /// Keysyms listed per keycode.
    pub per: usize,
    /// `per` keysyms for each keycode from `min`, in order.
    pub syms: Vec<u32>,
}

impl KeyMap {
    /// The keysyms of `keycode`; `None` if it is outside the map.
    pub fn of(&self, keycode: u8) -> Option<&[u32]> {
        let start = usize::from(keycode.checked_sub(self.min)?).checked_mul(self.per)?;
        self.syms.get(start..start.checked_add(self.per)?)
    }
}

/// Modifier state from XKB masks: `held` is base plus latched modifiers, `locked` the locked
/// ones. Shift, Control, Mod1 (Alt), Mod4 (Super) and Mod5 (AltGr, the usual home of
/// ISO_Level3_Shift) are read from `held`; Caps Lock is the `Lock` bit of `locked`.
pub fn mods_from_xkb(held: u16, locked: u16) -> OsMods {
    OsMods {
        shift: held & 0x01 != 0,
        ctrl: held & 0x04 != 0,
        alt: held & 0x08 != 0,
        alt_gr: held & 0x80 != 0,
        gui: held & 0x40 != 0,
        caps: locked & 0x02 != 0,
    }
}

/// What a keysym is for typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sym {
    /// No keysym: the key types nothing here.
    None,
    /// A character (possibly a control character such as Return).
    Char(char),
    /// A dead key, standing for this spacing accent.
    Dead(char),
}

/// The column of `(group, level)` in a keycode's keysym list, if the layout has it.
fn column(len: usize, group: usize, level: usize) -> Option<usize> {
    let col = match (group, level) {
        (0 | 1, 0 | 1) => 2 * group + level,
        (0, l) => 4 + (l - 2),
        // Higher levels of group 2 are not in the core mapping: use its first two.
        (1, l) => 2 + (l & 1),
        _ => return None,
    };
    (col < len).then_some(col)
}

/// The keysym for the held modifiers: Shift picks level 2, AltGr adds 2, and Caps Lock acts
/// as Shift on a key whose unshifted keysym is a letter. `0` is no symbol.
pub fn pick(syms: &[u32], group: usize, shift: bool, alt_gr: bool, caps: bool) -> u32 {
    let at = |g: usize, l: usize| column(syms.len(), g, l).map_or(0, |c| syms[c]);
    let letter = |s: u32| Keysym::new(s).key_char().is_some_and(char::is_alphabetic);
    let shift = shift ^ (caps && !alt_gr && letter(at(group, 0)));
    let level = usize::from(shift) + if alt_gr { 2 } else { 0 };
    // A key without that level types its next lower one; a layout without a second group
    // uses the first.
    [
        (group, level),
        (group, level & 1),
        (group, 0),
        (0, level),
        (0, level & 1),
        (0, 0),
    ]
    .into_iter()
    .map(|(g, l)| at(g, l))
    .find(|&s| s != 0)
    .unwrap_or(0)
}

/// Classifies a keysym.
pub fn classify(sym: u32) -> Sym {
    if sym == 0 {
        return Sym::None;
    }
    // XK_dead_grave .. XK_dead_cedilla, to the spacing accents.
    const DEAD: &[(u32, char)] = &[
        (0xFE50, '`'),
        (0xFE51, '´'),
        (0xFE52, '^'),
        (0xFE53, '~'),
        (0xFE54, '¯'),
        (0xFE55, '˘'),
        (0xFE56, '˙'),
        (0xFE57, '¨'),
        (0xFE58, '°'),
        (0xFE59, '˝'),
        (0xFE5A, 'ˇ'),
        (0xFE5B, '¸'),
    ];
    if let Some(d) = DEAD.iter().find(|d| d.0 == sym) {
        return Sym::Dead(d.1);
    }
    Keysym::new(sym).key_char().map_or(Sym::None, Sym::Char)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_slices_one_keycode() {
        let m = KeyMap {
            min: 8,
            per: 2,
            syms: vec![1, 2, 3, 4, 5, 6],
        };
        assert_eq!(m.of(8), Some(&[1, 2][..]));
        assert_eq!(m.of(10), Some(&[5, 6][..]));
        assert_eq!(m.of(11), None, "past the end");
        assert_eq!(m.of(7), None, "below the first keycode");
    }

    #[test]
    fn xkb_masks_become_modifiers() {
        assert_eq!(mods_from_xkb(0, 0), OsMods::default());
        let m = mods_from_xkb(0x01 | 0x04 | 0x08 | 0x40 | 0x80, 0x02);
        assert!(m.shift && m.ctrl && m.alt && m.gui && m.alt_gr && m.caps);
        assert!(!mods_from_xkb(0x02, 0).caps, "caps comes from the locked mask only");
        assert!(mods_from_xkb(0, 0x02).caps && !mods_from_xkb(0, 0x02).shift);
    }

    /// The German `q` key: q Q @ (AltGr) and the level-4 symbol.
    const Q_DE: [u32; 6] = [0x71, 0x51, 0, 0, 0x40, 0x51];
    /// The `1` key on a US layout: only two levels.
    const ONE_US: [u32; 2] = [0x31, 0x21];

    #[test]
    fn levels_follow_shift_altgr_and_caps() {
        assert_eq!(pick(&Q_DE, 0, false, false, false), 0x71);
        assert_eq!(pick(&Q_DE, 0, true, false, false), 0x51);
        assert_eq!(pick(&Q_DE, 0, false, true, false), 0x40, "AltGr+Q is @");
        assert_eq!(pick(&Q_DE, 0, false, false, true), 0x51, "Caps shifts a letter");
        assert_eq!(pick(&Q_DE, 0, true, false, true), 0x71, "Caps+Shift cancels");
        assert_eq!(pick(&ONE_US, 0, false, false, true), 0x31, "Caps leaves digits alone");
        assert_eq!(pick(&ONE_US, 0, false, true, false), 0x31, "no AltGr level: base key");
    }

    #[test]
    fn a_missing_second_group_falls_back_to_the_first() {
        assert_eq!(pick(&ONE_US, 1, false, false, false), 0x31);
    }

    #[test]
    fn keysyms_are_characters_dead_keys_or_nothing() {
        assert_eq!(classify(0x65), Sym::Char('e'));
        assert_eq!(classify(0x20AC), Sym::Char('€'), "latin and legacy keysyms");
        assert_eq!(classify(0x6C1), Sym::Char('а'), "Cyrillic_a");
        assert_eq!(classify(0x1000416), Sym::Char('Ж'), "direct Unicode keysym");
        assert_eq!(classify(0xFE51), Sym::Dead('´'));
        assert_eq!(classify(0xFF0D), Sym::Char('\r'), "Return is a control character");
        assert_eq!(classify(0), Sym::None);
    }
}
