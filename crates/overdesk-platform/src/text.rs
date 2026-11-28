// SPDX-License-Identifier: AGPL-3.0-only
//! The character a key press produces under the sender's own keyboard layout, for receivers
//! with no keyboard (an Android TV). The OS-specific part is a [`Layout`]; modifier tracking,
//! dead-key bookkeeping and the "is this text at all" rule live here, pure and tested.

use std::collections::BTreeSet;

use overdesk_proto::{PAGE_KEYBOARD, key_text_is_valid};

/// What the sender's layout makes of a key press that is not a plain `Key` for the receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyChar {
    /// Type this text instead of the key (see `Control::KeyText`).
    Text(String),
    /// A dead key: nothing to type yet, and the key itself must not be sent either; the
    /// next key's text carries the composed character.
    Pending,
}

/// Modifier state read from the OS, to seed a [`Typist`] that has been tracking keys the OS
/// never saw (see [`Typist::sync`]). Which physical key (left or right) is unknown.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OsMods {
    /// A Shift key is down.
    pub shift: bool,
    /// A Control key is down.
    pub ctrl: bool,
    /// Alt / Option (left) is down.
    pub alt: bool,
    /// Right Alt (AltGr, right Option) is down.
    pub alt_gr: bool,
    /// Windows / Command is down.
    pub gui: bool,
    /// Caps Lock is on.
    pub caps: bool,
}

/// Modifier groups, as bits of [`Mods::seeded`].
const G_SHIFT: u8 = 1;
/// Control group.
const G_CTRL: u8 = 2;
/// Left Alt group.
const G_ALT: u8 = 4;
/// Right Alt group.
const G_ALT_GR: u8 = 8;
/// Windows / Command group.
const G_GUI: u8 = 16;
/// Bit of [`Mods::held`] for the Caps Lock key (bits 0..=7 are usages 0xE0..=0xE7).
const CAPS_KEY: u16 = 1 << 8;

/// Which modifier keys are held, tracked from the keys themselves because a swallowed key
/// never reaches the OS's own modifier state. Held state is a set of usages, not a count:
/// a key-down for a usage already held is a repeat and changes nothing, and one key-up
/// always clears it, so a lost or repeated event cannot leave a modifier stuck.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Mods {
    /// Bit `n` (0..=7): modifier usage `0xE0 + n` is down; bit 8: the Caps Lock key is down.
    held: u16,
    /// Groups the OS reported held when tracking was seeded, for which the physical key is
    /// unknown; the first key event of the group takes over and clears it.
    seeded: u8,
    /// Caps Lock is on (toggled by each non-repeat press of the key).
    pub caps: bool,
}

impl Mods {
    /// Modifiers as the OS reports them (see [`Typist::sync`]).
    pub fn from_os(os: OsMods) -> Self {
        let bit = |on: bool, g: u8| if on { g } else { 0 };
        Self {
            held: 0,
            seeded: bit(os.shift, G_SHIFT)
                | bit(os.ctrl, G_CTRL)
                | bit(os.alt, G_ALT)
                | bit(os.alt_gr, G_ALT_GR)
                | bit(os.gui, G_GUI),
            caps: os.caps,
        }
    }

    /// The group of a modifier usage.
    fn group(usage: u32) -> Option<u8> {
        match usage {
            0xE1 | 0xE5 => Some(G_SHIFT),
            0xE0 | 0xE4 => Some(G_CTRL),
            0xE2 => Some(G_ALT),
            0xE6 => Some(G_ALT_GR),
            0xE3 | 0xE7 => Some(G_GUI),
            _ => None,
        }
    }

    /// Whether any key of `group` is held (or was reported held by the OS).
    fn any(&self, group: u8, usages: [u32; 2]) -> bool {
        self.seeded & group != 0 || usages.iter().any(|u| self.held & (1 << (u - 0xE0)) != 0)
    }

    /// Updates from `usage` (a keyboard-page HID usage); `false` if it is not a modifier.
    pub fn track(&mut self, usage: u32, pressed: bool) -> bool {
        let bit = match usage {
            0x39 => CAPS_KEY,
            0xE0..=0xE7 => 1 << (usage - 0xE0),
            _ => return false,
        };
        if usage == 0x39 && pressed && self.held & bit == 0 {
            self.caps = !self.caps; // only the first down toggles; a repeat does not
        }
        if let Some(g) = Self::group(usage) {
            self.seeded &= !g;
        }
        if pressed {
            self.held |= bit;
        } else {
            self.held &= !bit;
        }
        true
    }

    /// Whether Shift is held.
    pub fn shifted(&self) -> bool {
        self.any(G_SHIFT, [0xE1, 0xE5])
    }

    /// Whether Control is held.
    pub fn ctrl(&self) -> bool {
        self.any(G_CTRL, [0xE0, 0xE4])
    }

    /// Whether left Alt / Option is held.
    pub fn alt(&self) -> bool {
        self.any(G_ALT, [0xE2, 0xE2])
    }

    /// Whether right Alt (AltGr, right Option) is held.
    pub fn alt_gr(&self) -> bool {
        self.any(G_ALT_GR, [0xE6, 0xE6])
    }

    /// Whether the Windows / Command key is held.
    pub fn gui(&self) -> bool {
        self.any(G_GUI, [0xE3, 0xE7])
    }

    /// Whether the held modifiers make a key a shortcut rather than typing: Ctrl or the
    /// Windows/Command key, or Alt where Alt does not type (`option_types` is true on
    /// macOS, where Option makes characters). Right Alt types only where the layout has an
    /// AltGr level (`alt_gr_types`); otherwise it is a plain Alt. Ctrl beside a typing
    /// AltGr is not a shortcut: Windows reports AltGr as Ctrl+Alt.
    pub fn shortcut(&self, option_types: bool, alt_gr_types: bool) -> bool {
        let alt_gr = self.alt_gr() && alt_gr_types;
        let alt = self.alt() || (self.alt_gr() && !alt_gr_types);
        self.gui() || (self.ctrl() && !alt_gr) || (!option_types && alt)
    }
}

/// One layout lookup's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Raw {
    /// The key has no character under this layout.
    None,
    /// The key (with any dead key before it) produced this text, possibly control characters.
    Text(String),
    /// The key is a dead key; the layout remembers it for the next lookup.
    Dead,
}

/// The OS part: turns a key position into text under the current layout.
pub trait Layout {
    /// Whether Option/left Alt makes characters (macOS) rather than shortcuts.
    const OPTION_TYPES: bool;
    /// The text for `code` (a keyboard-page HID usage) with `mods` held, carrying dead-key
    /// state from the previous call.
    fn translate(&mut self, code: u32, mods: &Mods) -> Raw;
    /// Forgets any pending dead key.
    fn reset(&mut self);
    /// Re-reads the active layout where that must happen elsewhere (macOS: on the main
    /// queue); may complete later. The default does nothing.
    fn refresh(&mut self) {}
    /// The modifier keys and Caps Lock as the OS sees them now, or `None` if the platform
    /// cannot say. Seeds [`Typist::sync`].
    fn os_mods(&mut self) -> Option<OsMods> {
        None
    }
    /// Whether right Alt makes characters under the active layout (an AltGr level). Where
    /// it does not, right Alt is a plain Alt and keys typed with it are shortcuts.
    fn alt_gr_types(&mut self) -> bool {
        true
    }
}

/// Tracks modifiers and feeds a [`Layout`]; one per sender.
#[derive(Debug, Default)]
pub struct Typist<L> {
    /// Modifiers held.
    mods: Mods,
    /// Keyboard-page usages currently down, to tell an auto-repeat from a new press.
    down: BTreeSet<u32>,
    /// The dead key whose press armed the layout and has not been released, so its
    /// auto-repeats do not arm (and compose) it again.
    dead_key: Option<u32>,
    /// The OS layout lookup.
    layout: L,
}

impl<L: Layout> Typist<L> {
    /// A typist over `layout`.
    pub fn new(mut layout: L) -> Self {
        layout.refresh();
        Self {
            mods: Mods::default(),
            down: BTreeSet::new(),
            dead_key: None,
            layout,
        }
    }

    /// Re-reads the layout and replaces the tracked state with what the OS reports (see
    /// [`Typist::seed`]). Call it when translation starts.
    pub fn sync(&mut self) {
        self.layout.refresh();
        let os = self.layout.os_mods().unwrap_or_default();
        self.seed(os);
    }

    /// Replaces the tracked state with `os`: modifiers, Caps Lock, and no keys down or
    /// half-typed. Needed because keys pressed
    /// while input is swallowed never reach the OS: its Caps Lock stays put while ours
    /// toggles, so the two drift, and a modifier held before the cursor crossed over is
    /// unknown to us. Re-syncing at every start bounds the drift to one session.
    pub fn seed(&mut self, os: OsMods) {
        self.mods = Mods::from_os(os);
        self.down.clear();
        self.dead_key = None;
        self.layout.reset();
    }

    /// Feeds one captured key. Call it for every key so modifiers stay right; pass
    /// `translate = false` while input is not going to a receiver that wants text (it also
    /// drops a half-typed dead key). `Some` means: send this instead of the `Key`.
    pub fn key(&mut self, code: u32, pressed: bool, translate: bool) -> Option<KeyChar> {
        if code & 0xFFFF_0000 != PAGE_KEYBOARD {
            return None;
        }
        // A down for a usage already down is the OS's auto-repeat.
        let repeat = if pressed {
            !self.down.insert(code)
        } else {
            self.down.remove(&code);
            if self.dead_key == Some(code) {
                self.dead_key = None;
            }
            false
        };
        if self.mods.track(code, pressed) {
            return None;
        }
        if !translate {
            self.layout.reset();
            self.dead_key = None;
            return None;
        }
        if !pressed {
            return None;
        }
        if repeat && self.dead_key == Some(code) {
            return Some(KeyChar::Pending); // already armed; arming again would compose twice
        }
        let alt_gr_types = self.layout.alt_gr_types();
        if self.mods.shortcut(L::OPTION_TYPES, alt_gr_types) {
            self.layout.reset();
            self.dead_key = None;
            return None;
        }
        let typed = self.layout.translate(code, &self.mods);
        self.dead_key = (typed == Raw::Dead).then_some(code);
        match typed {
            Raw::Dead => Some(KeyChar::Pending),
            Raw::Text(t) if key_text_is_valid(&t) => Some(KeyChar::Text(t)),
            // Enter, Backspace, Tab, arrows and the like come back as control characters.
            Raw::Text(_) | Raw::None => None,
        }
    }
}

/// Composes a dead key with the next base character the way Windows and X11 layouts do:
/// the accented letter if there is one, else the accent followed by the letter; space
/// gives the accent alone. `accent` is the spacing accent the dead key stands for.
pub fn compose(accent: char, next: char) -> String {
    if next == ' ' {
        return accent.to_string();
    }
    // (spacing accent, letters, their accented forms), same order.
    const TABLE: &[(char, &str, &str)] = &[
        ('´', "aeiouyAEIOUYcCnNsSzZ", "áéíóúýÁÉÍÓÚÝćĆńŃśŚźŹ"),
        ('`', "aeiouAEIOU", "àèìòùÀÈÌÒÙ"),
        ('^', "aeiouAEIOU", "âêîôûÂÊÎÔÛ"),
        ('¨', "aeiouyAEIOU", "äëïöüÿÄËÏÖÜ"),
        ('~', "anoANOiI", "ãñõÃÑÕĩĨ"),
        ('¸', "cCsStT", "çÇşŞţŢ"),
        ('°', "aAuU", "åÅůŮ"),
    ];
    TABLE
        .iter()
        .find(|t| t.0 == accent)
        .and_then(|t| t.1.chars().position(|c| c == next).and_then(|i| t.2.chars().nth(i)))
        .map_or_else(|| format!("{accent}{next}"), String::from)
}

/// `text` typed after a dead key `pending` (taken, so it is used once): the first character
/// composes with the accent, the rest follows.
pub fn compose_pending(pending: &mut Option<char>, text: String) -> String {
    let Some(accent) = pending.take() else { return text };
    let mut chars = text.chars();
    let first = chars.next().map_or_else(String::new, |c| compose(accent, c));
    first + chars.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use overdesk_proto::MAX_KEY_TEXT_CHARS;

    /// Pretends: usage 0x04 types `a`/`A`, 0x05 is a dead acute, 0x28 is Enter (a control char).
    #[derive(Default)]
    struct Fake {
        dead: bool,
        resets: u32,
        dead_presses: u32,
        alt_gr_types: bool,
    }

    impl Layout for Fake {
        const OPTION_TYPES: bool = false;
        fn translate(&mut self, code: u32, mods: &Mods) -> Raw {
            let base = match code {
                0x05 => {
                    self.dead_presses += 1;
                    self.dead = true;
                    return Raw::Dead;
                }
                0x04 => {
                    if mods.shifted() {
                        'A'
                    } else {
                        'a'
                    }
                }
                0x28 => '\r',
                _ => return Raw::None,
            };
            let out = if std::mem::take(&mut self.dead) {
                compose('´', base)
            } else {
                base.to_string()
            };
            Raw::Text(out)
        }
        fn reset(&mut self) {
            self.dead = false;
            self.resets += 1;
        }
        fn alt_gr_types(&mut self) -> bool {
            self.alt_gr_types
        }
    }

    fn typist() -> Typist<Fake> {
        Typist::new(Fake {
            alt_gr_types: true,
            ..Fake::default()
        })
    }

    #[test]
    fn letters_follow_shift_and_releases_are_not_text() {
        let mut t = typist();
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("a".into())));
        assert_eq!(t.key(0x04, false, true), None);
        assert_eq!(t.key(0xE1, true, true), None, "a modifier is a plain key");
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("A".into())));
        t.key(0xE1, false, true);
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("a".into())));
    }

    #[test]
    fn shortcuts_and_control_keys_stay_keys() {
        let mut t = typist();
        assert_eq!(t.key(0x28, true, true), None, "Enter");
        assert_eq!(t.key(0x4F, true, true), None, "arrow: no character");
        t.key(0xE0, true, true);
        assert_eq!(t.key(0x04, true, true), None, "Ctrl+A");
        t.key(0xE0, false, true);
        t.key(0xE3, true, true);
        assert_eq!(t.key(0x04, true, true), None, "Cmd+A");
        t.key(0xE3, false, true);
        t.key(0xE2, true, true);
        assert_eq!(t.key(0x04, true, true), None, "Alt+A where Alt does not type");
        t.key(0xE2, false, true);
        // AltGr arrives as Ctrl+Alt on Windows: still typing.
        t.key(0xE0, true, true);
        t.key(0xE6, true, true);
        assert!(t.key(0x04, true, true).is_some());
    }

    #[test]
    fn a_dead_key_sends_nothing_and_composes_with_the_next() {
        let mut t = typist();
        assert_eq!(t.key(0x05, true, true), Some(KeyChar::Pending));
        assert_eq!(t.key(0x05, false, true), None);
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("á".into())));
        assert_eq!(
            t.key(0x04, true, true),
            Some(KeyChar::Text("a".into())),
            "state is consumed"
        );
    }

    #[test]
    fn leaving_the_receiver_drops_a_pending_dead_key() {
        let mut t = typist();
        t.key(0x05, true, true);
        t.key(0x04, true, false);
        assert_eq!(t.layout.resets, 1);
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("a".into())));
    }

    #[test]
    fn compose_falls_back_to_accent_then_letter() {
        assert_eq!(compose('´', 'e'), "é");
        assert_eq!(compose('¨', 'U'), "Ü");
        assert_eq!(compose('´', ' '), "´");
        assert_eq!(compose('´', 'k'), "´k");
        assert!(key_text_is_valid(&compose('~', 'n')) && "é".chars().count() <= MAX_KEY_TEXT_CHARS);
    }

    #[test]
    fn a_pending_accent_is_used_once() {
        let mut p = Some('´');
        assert_eq!(compose_pending(&mut p, "e".into()), "é");
        assert_eq!(compose_pending(&mut p, "e".into()), "e");
    }

    #[test]
    fn caps_toggles_on_each_press() {
        let mut m = Mods::default();
        m.track(0x39, true);
        m.track(0x39, false);
        assert!(m.caps);
        m.track(0x39, true);
        assert!(!m.caps);
    }

    #[test]
    fn many_shift_downs_and_one_up_leave_shift_released() {
        let mut t = typist();
        for _ in 0..30 {
            t.key(0xE1, true, true);
        }
        t.key(0xE1, false, true);
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("a".into())));
    }

    #[test]
    fn both_shifts_held_stay_shifted_until_both_are_up() {
        let mut t = typist();
        t.key(0xE1, true, true);
        t.key(0xE5, true, true);
        t.key(0xE1, false, true);
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("A".into())));
        t.key(0xE5, false, true);
        t.key(0x04, false, true);
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("a".into())));
    }

    #[test]
    fn caps_toggles_once_however_long_it_is_held() {
        let mut m = Mods::default();
        for _ in 0..5 {
            m.track(0x39, true);
        }
        assert!(m.caps, "five auto-repeats toggled once");
        m.track(0x39, false);
        m.track(0x39, true);
        assert!(!m.caps);
    }

    #[test]
    fn holding_a_letter_repeats_it_but_holding_a_dead_key_arms_once() {
        let mut t = typist();
        for _ in 0..3 {
            assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("a".into())), "repeats type");
        }
        t.key(0x04, false, true);
        assert_eq!(t.key(0x05, true, true), Some(KeyChar::Pending));
        assert_eq!(t.key(0x05, true, true), Some(KeyChar::Pending), "repeat");
        assert_eq!(t.key(0x05, true, true), Some(KeyChar::Pending), "repeat");
        assert_eq!(t.layout.dead_presses, 1, "the layout saw the dead key once");
        t.key(0x05, false, true);
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("á".into())));
        t.key(0x04, false, true);
        // Released and pressed again: a new press arms again.
        assert_eq!(t.key(0x05, true, true), Some(KeyChar::Pending));
        assert_eq!(t.layout.dead_presses, 2);
    }

    #[test]
    fn syncing_seeds_what_the_os_holds_and_a_release_takes_over() {
        let mut t = typist();
        t.seed(OsMods {
            shift: true,
            caps: true,
            ..OsMods::default()
        });
        assert!(t.mods.caps && t.mods.shifted());
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("A".into())));
        t.key(0xE5, false, true); // the user lets go of (right) Shift
        assert!(!t.mods.shifted(), "a seeded modifier cannot stick");
        assert!(t.mods.caps, "Caps Lock stays on");
    }

    #[test]
    fn syncing_forgets_a_diverged_caps_and_stale_keys() {
        let mut t = typist();
        t.key(0x39, true, true); // Caps pressed while driving: the OS never saw it
        t.key(0x39, false, true);
        t.key(0x04, true, true); // 'a' never released
        t.seed(OsMods::default());
        assert!(!t.mods.caps);
        assert_eq!(t.key(0x04, true, true), Some(KeyChar::Text("a".into())));
        assert!(t.down.contains(&0x04) && t.down.len() == 1);
    }

    #[test]
    fn right_alt_is_a_shortcut_only_without_an_altgr_level() {
        let mut m = Mods::default();
        m.track(0xE6, true);
        assert!(!m.shortcut(false, true), "AltGr types");
        assert!(m.shortcut(false, false), "no AltGr level: it is Alt");
        m.track(0xE0, true);
        assert!(!m.shortcut(false, true), "Ctrl+AltGr is how Windows sends AltGr");
        assert!(m.shortcut(false, false));
    }
}
