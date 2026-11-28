// SPDX-License-Identifier: AGPL-3.0-only
//! Characters from the X server's keyboard mapping, in pure Rust (no libxkbcommon): the
//! core `GetKeyboardMapping` for the key, the active XKB group, and [`keysym`](super::keysym) to choose
//! the level. The mapping is fetched once and refreshed on `MappingNotify`; the active group
//! follows XKB state-notify events, so a layout switched at run time is followed with no
//! round trip per key. A failed connection is not retried for a few seconds.
//!
//! Wayland offers no keymap through the portals, so there this always answers "no
//! character" and the receiver falls back to its own key map for the key.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use super::ffi::session_is_x11;
use super::keymap::code_to_keycode;
use super::keysym::{KeyMap, Sym, classify, mods_from_xkb, pick};
use super::x11::connect;
use crate::text::{Layout, Mods, OsMods, Raw, compose_pending};

/// How long to leave the X server alone after a failed connection before trying again, so a
/// missing or restarting server costs one attempt per interval, not one per key.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// A live connection and what it has learned. A key press costs no round trip: the keyboard
/// mapping is fetched once and the XKB group follows state-notify events, both drained from
/// the socket without waiting.
struct Live {
    /// The connection, with XKB state and keyboard-change events selected where XKB exists.
    conn: RustConnection,
    /// The whole keyboard mapping; `None` after the server reported a change, until refetched.
    map: Option<KeyMap>,
    /// The active XKB group.
    group: usize,
}

impl Live {
    /// Connects and reads the initial group; `None` if the server cannot be reached.
    fn connect() -> Option<Self> {
        let (conn, _) = connect().ok()?;
        let mut group = 0;
        // Without XKB the first group is used and never changes.
        if xkb::use_extension(&conn, 1, 0).ok()?.reply().is_ok_and(|r| r.supported) {
            let all = xkb::EventType::STATE_NOTIFY | xkb::EventType::NEW_KEYBOARD_NOTIFY | xkb::EventType::MAP_NOTIFY;
            conn.xkb_select_events(
                xkb::ID::USE_CORE_KBD.into(),
                xkb::EventType::from(0u16),
                all,
                xkb::MapPart::from(0u16),
                xkb::MapPart::from(0u16),
                &xkb::SelectEventsAux::new(),
            )
            .ok()?;
            group = usize::from(u8::from(
                conn.xkb_get_state(xkb::ID::USE_CORE_KBD.into())
                    .ok()?
                    .reply()
                    .ok()?
                    .group,
            ));
        }
        Some(Self { conn, map: None, group })
    }

    /// Applies the events that arrived since last time; `Err` if the connection is broken.
    fn drain(&mut self) -> Result<(), ()> {
        while let Some(event) = self.conn.poll_for_event().map_err(|_| ())? {
            match event {
                Event::XkbStateNotify(s) => self.group = usize::from(u8::from(s.group)),
                Event::MappingNotify(_) | Event::XkbNewKeyboardNotify(_) | Event::XkbMapNotify(_) => self.map = None,
                _ => {}
            }
        }
        Ok(())
    }

    /// The keysyms of `keycode` and the active group; `Err` if the connection is broken.
    fn lookup(&mut self, keycode: u8) -> Result<Option<(Vec<u32>, usize)>, ()> {
        self.drain()?;
        if self.map.is_none() {
            let (min, max) = (self.conn.setup().min_keycode, self.conn.setup().max_keycode);
            let r = self
                .conn
                .get_keyboard_mapping(min, max - min + 1)
                .map_err(|_| ())?
                .reply()
                .map_err(|_| ())?;
            self.map = Some(KeyMap {
                min,
                per: usize::from(r.keysyms_per_keycode),
                syms: r.keysyms,
            });
        }
        Ok(self
            .map
            .as_ref()
            .and_then(|m| m.of(keycode))
            .map(|syms| (syms.to_vec(), self.group)))
    }

    /// The modifier state the server holds; `Err` if the connection is broken.
    fn os_mods(&mut self) -> Result<OsMods, ()> {
        let r = self
            .conn
            .xkb_get_state(xkb::ID::USE_CORE_KBD.into())
            .map_err(|_| ())?
            .reply()
            .map_err(|_| ())?;
        let held = u16::from(r.base_mods) | u16::from(r.latched_mods);
        Ok(mods_from_xkb(held, u16::from(r.locked_mods)))
    }
}

/// The X11 layout lookup.
#[derive(Default)]
pub struct X11Layout {
    /// The connection to the X server, made at the first lookup and dropped on any request
    /// error, so a restarted server is picked up by the next key.
    live: Option<Live>,
    /// After a failed connect, no new attempt before this moment.
    retry_at: Option<Instant>,
    /// Spacing accent of the dead key pressed last, waiting for the next character.
    pending: Option<char>,
}

impl X11Layout {
    /// Runs `f` on the connection, making it first if due; `None` on Wayland, while backing
    /// off after a failed connect, or when `f` fails (which also drops the connection).
    fn with_live<T>(&mut self, f: impl FnOnce(&mut Live) -> Result<T, ()>) -> Option<T> {
        if self.live.is_none() {
            if !session_is_x11() || self.retry_at.is_some_and(|t| Instant::now() < t) {
                return None;
            }
            self.live = Live::connect();
            self.retry_at = self.live.is_none().then(|| Instant::now() + RETRY_AFTER);
        }
        let out = f(self.live.as_mut()?);
        if out.is_err() {
            self.live = None;
        }
        out.ok()
    }
}

impl Layout for X11Layout {
    const OPTION_TYPES: bool = false;

    fn translate(&mut self, code: u32, mods: &Mods) -> Raw {
        let Some(keycode) = code_to_keycode(code) else {
            return Raw::None;
        };
        let Some(Some((syms, group))) = self.with_live(|l| l.lookup(keycode)) else {
            return Raw::None;
        };
        match classify(pick(&syms, group, mods.shifted(), mods.alt_gr(), mods.caps)) {
            Sym::None => Raw::None,
            Sym::Dead(accent) => {
                self.pending = Some(accent);
                Raw::Dead
            }
            Sym::Char(c) => Raw::Text(compose_pending(&mut self.pending, c.to_string())),
        }
    }

    fn reset(&mut self) {
        self.pending = None;
    }

    fn os_mods(&mut self) -> Option<OsMods> {
        self.with_live(Live::os_mods)
    }
}
