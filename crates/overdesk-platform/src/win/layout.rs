// SPDX-License-Identifier: AGPL-3.0-only
//! Characters from the Windows keyboard layout: `ToUnicodeEx` with the layout of the
//! foreground thread and a key-state array built from the tracked modifiers.
//!
//! Dead keys: `ToUnicode` normally keeps a pending dead key in system state, so asking
//! "what would this key type" corrupts the next real keystroke of whoever owns that state
//! (the classic double-call problem). Bit 2 of `wFlags` (Windows 10 1607+) leaves the state
//! untouched, and we always set it: a dead key then reports its spacing accent with result
//! `-1`, we remember that accent ourselves and compose it with the next key's character
//! through [`compose`](crate::text::compose). Nothing is ever left pending in the system.
#![allow(unsafe_code)] // Win32 FFI; each unsafe block states its invariant.

use std::ptr::null_mut;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyboardLayout, HKL, MapVirtualKeyExW, ToUnicodeEx,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::keymap::code_to_scan;
use crate::text::{Layout, Mods, OsMods, Raw, compose_pending};

/// `MAPVK_VSC_TO_VK_EX`: scan code to a left/right-aware virtual key.
const MAPVK_VSC_TO_VK_EX: u32 = 3;
/// `ToUnicodeEx` flag: do not change the keyboard (dead-key) state.
const KEEP_STATE: u32 = 4;
/// Virtual keys that matter to character production.
const VK_SHIFT: usize = 0x10;
/// `VK_CONTROL`.
const VK_CONTROL: usize = 0x11;
/// `VK_MENU` (Alt).
const VK_MENU: usize = 0x12;
/// `VK_CAPITAL` (Caps Lock; bit 0 is the toggle).
const VK_CAPITAL: usize = 0x14;
/// `VK_NUMLOCK`.
const VK_NUMLOCK: usize = 0x90;
/// `VK_RMENU` (right Alt).
const VK_RMENU: i32 = 0xA5;

/// The Windows layout lookup.
#[derive(Debug, Default)]
pub struct WinLayout {
    /// Spacing accent of the dead key pressed last, waiting for the next character.
    pending: Option<char>,
    /// Whether the keyboard layout (by handle) probed last has an AltGr level.
    alt_gr: Option<(usize, bool)>,
}

/// The layout of the foreground thread (this thread's if there is no foreground window).
fn foreground_hkl() -> HKL {
    // SAFETY: both calls take no pointers we own beyond a null "don't care" process-id out
    // pointer; a null window handle makes thread 0, meaning the calling thread.
    unsafe { GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), null_mut())) }
}

/// Whether probing a layout with Ctrl+Alt showed an AltGr level: some key made a printable
/// character or a dead key. `results` are `ToUnicodeEx` return values with the first UTF-16
/// unit written. Without an AltGr level, Ctrl+Alt+letter yields only control characters or nothing.
fn has_alt_gr_level(results: impl IntoIterator<Item = (i32, u16)>) -> bool {
    results
        .into_iter()
        .any(|(n, first)| n == -1 || (n >= 1 && first >= 0x20 && first != 0x7F))
}

/// Probes `hkl` for an AltGr level: a few dozen letter, digit and punctuation keys with
/// Ctrl+Alt down. The "keep state" flag leaves the system's dead-key state alone.
fn probe_alt_gr(hkl: HKL) -> bool {
    let mut state = [0u8; 256];
    state[VK_CONTROL] = 0x80;
    state[VK_MENU] = 0x80;
    let results = (0x04u32..=0x27).chain(0x2D..=0x38).filter_map(|usage| {
        let scan = code_to_scan(usage)?;
        let mut buf = [0u16; 8];
        // SAFETY: `state` is the 256 bytes the API reads and `buf` is writable for the 8
        // units we declare; KEEP_STATE means the call changes no system state.
        let n = unsafe {
            let vk = MapVirtualKeyExW(u32::from(scan), MAPVK_VSC_TO_VK_EX, hkl);
            if vk == 0 {
                return None;
            }
            ToUnicodeEx(
                vk,
                u32::from(scan) & 0xFF,
                state.as_ptr(),
                buf.as_mut_ptr(),
                8,
                KEEP_STATE,
                hkl,
            )
        };
        Some((n, buf[0]))
    });
    has_alt_gr_level(results)
}

/// Whether the virtual key `vk` is physically down right now.
fn down(vk: i32) -> bool {
    // SAFETY: a plain query of key state by virtual-key code; no pointers involved.
    unsafe { GetAsyncKeyState(vk) as u16 & 0x8000 != 0 }
}

/// The modifier keys and Caps Lock as the OS sees them now. Safe from any thread.
pub fn os_mods() -> OsMods {
    let alt_gr = down(VK_RMENU);
    OsMods {
        shift: down(VK_SHIFT as i32),
        ctrl: down(VK_CONTROL as i32),
        alt: down(VK_MENU as i32) && !alt_gr,
        alt_gr,
        gui: down(0x5B) || down(0x5C), // VK_LWIN, VK_RWIN
        // Tracked by the capture thread while capturing: `GetKeyState` is stale here (see `caps_lock_on`).
        caps: super::ffi::caps_lock_on(),
    }
}

/// The 256-byte key-state array for `mods`; AltGr is Ctrl+Alt, as Windows layouts expect.
fn key_state(mods: &Mods) -> [u8; 256] {
    let mut k = [0u8; 256];
    if mods.shifted() {
        k[VK_SHIFT] = 0x80;
    }
    if mods.alt_gr() {
        k[VK_CONTROL] = 0x80;
        k[VK_MENU] = 0x80;
    }
    k[VK_CAPITAL] = u8::from(mods.caps);
    k[VK_NUMLOCK] = 1;
    k
}

impl Layout for WinLayout {
    const OPTION_TYPES: bool = false;

    fn translate(&mut self, code: u32, mods: &Mods) -> Raw {
        let Some(scan) = code_to_scan(code) else {
            return Raw::None;
        };
        let state = key_state(mods);
        let mut buf = [0u16; 8];
        let hkl = foreground_hkl();
        // SAFETY: `state` is the 256 bytes the API reads and `buf` is writable for the 8
        // units we declare. KEEP_STATE means the call changes no system state.
        let n = unsafe {
            let vk = MapVirtualKeyExW(u32::from(scan), MAPVK_VSC_TO_VK_EX, hkl);
            if vk == 0 {
                return Raw::None;
            }
            ToUnicodeEx(
                vk,
                u32::from(scan) & 0xFF, // bit 15 would mean key-up; the E0 prefix is not part of it
                state.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as i32,
                KEEP_STATE,
                hkl,
            )
        };
        let text = String::from_utf16_lossy(&buf[..usize::try_from(n.unsigned_abs()).unwrap_or(0).min(buf.len())]);
        match n {
            // A dead key: `text` is its spacing accent.
            -1 => match text.chars().next() {
                Some(accent) => {
                    self.pending = Some(accent);
                    Raw::Dead
                }
                None => Raw::None,
            },
            1.. => Raw::Text(compose_pending(&mut self.pending, text)),
            _ => Raw::None,
        }
    }

    fn reset(&mut self) {
        self.pending = None;
    }

    fn os_mods(&mut self) -> Option<OsMods> {
        Some(os_mods())
    }

    /// Heuristic: the layout has an AltGr level if Ctrl+Alt makes printable characters
    /// (see `probe_alt_gr`), probed once per layout handle and cached. Layouts without one
    /// (US, UK) keep right Alt as a plain Alt, so Alt shortcuts work from either Alt key.
    fn alt_gr_types(&mut self) -> bool {
        let hkl = foreground_hkl();
        let key = hkl as usize;
        match self.alt_gr {
            Some((k, has)) if k == key => has,
            _ => {
                let has = probe_alt_gr(hkl);
                self.alt_gr = Some((key, has));
                has
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_altgr_level_shows_as_printable_or_dead() {
        assert!(has_alt_gr_level([(1, u16::from(b'@'))]), "German AltGr+Q");
        assert!(has_alt_gr_level([(0, 0), (-1, u16::from(b'`'))]), "a dead key counts");
    }

    #[test]
    fn control_characters_and_nothing_are_not_an_altgr_level() {
        assert!(!has_alt_gr_level([]));
        assert!(
            !has_alt_gr_level([(1, 0x01), (0, 0), (1, 0x7F), (2, 0x1B)]),
            "Ctrl+letter only"
        );
    }
}
