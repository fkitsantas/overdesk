// SPDX-License-Identifier: AGPL-3.0-only
//! The small files in the store folder: which side each peer's screen is on, and each paired
//! computer's name.

use std::collections::BTreeMap;

use overdesk_proto::PeerId;

use overdesk_net::session::sanitize_name;
use overdesk_net::transport::NetError;

use super::Side;

/// The file in the store folder that remembers which side each peer's screen is on.
pub(super) const SIDES_FILE: &str = "sides.txt";
/// The file in the store folder that remembers each paired computer's name (hex id, tab, name).
pub(super) const NAMES_FILE: &str = "names.txt";

/// The saved side of `peer`'s screen, `Right` if none was saved or the file is unreadable.
pub(super) fn load_side(dir: &std::path::Path, peer: &PeerId) -> Side {
    let key = peer.to_hex();
    let text = std::fs::read_to_string(dir.join(SIDES_FILE)).unwrap_or_default();
    let found = text
        .lines()
        .find_map(|l| l.strip_prefix(key.as_str())?.strip_prefix(' '));
    found.and_then(|word| word.parse().ok()).unwrap_or(Side::Right)
}

/// Remembers `side` for `peer`: rewrites the file via a temp file and rename, so a crash
/// never leaves it half-written.
///
/// # Errors
///
/// [`NetError::Io`] if it cannot be written; losing it only costs the default side.
pub(super) fn save_side(dir: &std::path::Path, peer: &PeerId, side: Side) -> Result<(), NetError> {
    let key = peer.to_hex();
    let path = dir.join(SIDES_FILE);
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut text: String = old
        .lines()
        .filter(|l| !l.starts_with(key.as_str()))
        .map(|l| format!("{l}\n"))
        .collect();
    text += &format!("{key} {}\n", side.as_str());
    write_atomic(&path, &text)
}

/// Replaces `path`'s contents the way the pins are saved (private, synced, atomic).
fn write_atomic(path: &std::path::Path, text: &str) -> Result<(), NetError> {
    overdesk_net::store::write_private(path, text.as_bytes())
}

/// `name` as a peer's name is made safe to show (no control or invisible characters, bounded),
/// without surrounding spaces.
pub(super) fn clean_name(name: &str) -> String {
    sanitize_name(name).trim().to_owned()
}

/// Reads `names.txt` text: one `hex-id<TAB>name` per line; malformed lines are skipped.
pub(super) fn parse_names(text: &str) -> BTreeMap<PeerId, String> {
    text.lines()
        .filter_map(|l| {
            let (id, name) = l.split_once('\t')?;
            let name = clean_name(name);
            (!name.is_empty()).then_some((PeerId::from_hex(id)?, name))
        })
        .collect()
}

/// The text of `names.txt` for `names`.
pub(super) fn render_names(names: &BTreeMap<PeerId, String>) -> String {
    names
        .iter()
        .map(|(id, n)| format!("{}\t{}\n", id.to_hex(), clean_name(n)))
        .collect()
}

/// The saved names, empty if the file is missing or unreadable.
pub fn load_names(dir: &std::path::Path) -> BTreeMap<PeerId, String> {
    parse_names(&std::fs::read_to_string(dir.join(NAMES_FILE)).unwrap_or_default())
}

/// Saves `names`, atomically.
///
/// # Errors
///
/// [`NetError::Io`] if it cannot be written; losing it only costs the names until they are heard again.
pub(super) fn save_names(dir: &std::path::Path, names: &BTreeMap<PeerId, String>) -> Result<(), NetError> {
    write_atomic(&dir.join(NAMES_FILE), &render_names(names))
}

/// Remembers `name` for `id` in `names` and in `names.txt` as heard from the peer itself over
/// its authenticated link, replacing whatever was known. `Ok(true)` if that changed anything.
///
/// # Errors
///
/// [`NetError::Io`] if the change could not be saved; `names` has it all the same.
pub fn learn_name(
    dir: &std::path::Path,
    names: &mut BTreeMap<PeerId, String>,
    id: PeerId,
    name: &str,
) -> Result<bool, NetError> {
    let name = clean_name(name);
    let changed = !name.is_empty() && names.get(&id) != Some(&name);
    if changed {
        names.insert(id, name);
        save_names(dir, names)?;
    }
    Ok(changed)
}

/// Forgets the name of `id` in `names` and in `names.txt`.
///
/// # Errors
///
/// [`NetError::Io`] if the change could not be saved; `names` has forgotten it all the same.
pub fn forget_name(dir: &std::path::Path, names: &mut BTreeMap<PeerId, String>, id: PeerId) -> Result<(), NetError> {
    if names.remove(&id).is_some() {
        save_names(dir, names)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_survive_hostile_input() {
        let id = PeerId([0xab; 16]);
        let names = BTreeMap::from([(id, "Office PC".to_owned()), (PeerId([1; 16]), "Zed".to_owned())]);
        assert_eq!(parse_names(&render_names(&names)), names);
        // Bad ids, short ids, empty names, no tab and control characters are dropped or stripped.
        let key = id.to_hex();
        let text = format!("zz\tx\n{}\tshort\n{key}\t\n{key}\ta\rb\x1bc \nno tab here\n", &key[2..]);
        assert_eq!(parse_names(&text), BTreeMap::from([(id, "abc".to_owned())]));
        assert_eq!(
            PeerId::from_hex("é".repeat(16).as_str()),
            None,
            "non-ASCII of the right byte length"
        );
        assert_eq!(
            PeerId::from_hex(&format!("+{}", &key[1..])),
            None,
            "a sign is not a digit"
        );
        assert_eq!(clean_name("  a\tb\nc  "), "abc");
        assert_eq!(
            clean_name("a\u{202e}b"),
            "ab",
            "direction overrides are invisible characters"
        );
    }

    #[test]
    fn learned_names_are_saved_and_forgotten() {
        let dir = std::env::temp_dir().join(format!("overdesk-learn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (id, mut names) = (PeerId([3; 16]), BTreeMap::new());
        assert!(learn_name(&dir, &mut names, id, " Desk\n").unwrap());
        assert!(!learn_name(&dir, &mut names, id, "Desk").unwrap(), "unchanged");
        assert!(!learn_name(&dir, &mut names, id, "  ").unwrap(), "empty is ignored");
        assert_eq!(load_names(&dir), BTreeMap::from([(id, "Desk".to_owned())]));
        forget_name(&dir, &mut names, id).unwrap();
        assert!(load_names(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sides_round_trip_per_peer() {
        let dir = std::env::temp_dir().join(format!("overdesk-sides-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (PeerId([1; 16]), PeerId([2; 16]));
        assert_eq!(load_side(&dir, &a), Side::Right);
        save_side(&dir, &a, Side::Left).unwrap();
        save_side(&dir, &b, Side::Right).unwrap();
        save_side(&dir, &b, Side::Left).unwrap();
        assert_eq!((load_side(&dir, &a), load_side(&dir, &b)), (Side::Left, Side::Left));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
