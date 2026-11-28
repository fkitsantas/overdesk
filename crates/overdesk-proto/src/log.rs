// SPDX-License-Identifier: AGPL-3.0-only
//! The one way a process tells the user (and, through a bug report, us) what it is doing:
//! timestamped lines in `<store dir>/logs/overdesk.log`, rotated at [`LIMIT`] to `overdesk.log.1`.
//! Warnings also go to stderr; until a binary calls [`init`] (and whenever the file cannot be
//! written) every line does.
//! Library crates never print; they return the condition and the binary hands it here.
//!
//! # What may be logged
//!
//! Lifecycle and outcomes only: versions, OS names, display sizes, short peer ids, device
//! names, addresses, phases and reasons. Never keystrokes, pointer coordinates per event,
//! clipboard contents, PINs or pairing codes, licence keys, lease tokens or email addresses:
//! callers pass this module only text they built from those safe parts, never a `{:?}` of a
//! state, command or configuration type that carries any of them. The file is read by whoever
//! the user sends it to, so assume it is public.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::{PeerId, hex};

/// The current log file's name.
pub const FILE: &str = "overdesk.log";
/// The previous log file's name, replaced at every rotation.
pub const OLD: &str = "overdesk.log.1";
/// Size at which the current file becomes the old one.
pub const LIMIT: u64 = 1 << 20;

/// The folder of the log files, set once by [`init`].
static DIR: OnceLock<PathBuf> = OnceLock::new();
/// Held while a line is appended, so threads never interleave a rotation with a write.
static WRITING: Mutex<()> = Mutex::new(());

/// Starts logging to `<store>/logs/`. Call once, first thing in a binary; later calls and a
/// folder that cannot be created change nothing (lines then go to stderr).
pub fn init(store: &Path) {
    let dir = store.join("logs");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = DIR.set(dir);
    }
}

/// The folder [`init`] chose, if it did.
pub fn dir() -> Option<&'static Path> {
    DIR.get().map(PathBuf::as_path)
}

/// Records a problem that does not stop the process.
pub fn warn(what: impl std::fmt::Display) {
    line("WARN", &what.to_string());
}

/// Records something that happened: a lifecycle event or the outcome of an action.
pub fn info(what: impl std::fmt::Display) {
    line("INFO", &what.to_string());
}

/// The first four bytes of `id` in hex: enough to tell peers apart in a log, not to name one.
pub fn short_id(id: &PeerId) -> String {
    hex::hex(&id.0[..4])
}

/// Collapses a run of identical lines: the first is written, the rest are only counted.
#[derive(Default)]
struct Collapse {
    /// The level and text of the last line seen.
    last: Option<(String, String)>,
    /// How many identical lines followed it and were not written.
    repeats: u64,
}

impl Collapse {
    /// The lines to write for a new `what` at `level`: nothing for a repeat, else the pending
    /// repeat count (if any) and then the new line.
    fn push(&mut self, level: &str, what: &str) -> Vec<(String, String)> {
        if self.last.as_ref().is_some_and(|(l, w)| l == level && w == what) {
            self.repeats += 1;
            return Vec::new();
        }
        let mut out: Vec<(String, String)> = self.take().into_iter().collect();
        out.push((level.to_owned(), what.to_owned()));
        self.last = Some((level.to_owned(), what.to_owned()));
        out
    }

    /// The pending repeat count as a line, and the count reset.
    fn take(&mut self) -> Option<(String, String)> {
        let n = std::mem::take(&mut self.repeats);
        (n > 0).then(|| ("INFO".to_owned(), format!("last line repeated {n} times")))
    }
}

/// What has been logged last, for [`Collapse`]; held while writing so lines stay in order.
static LAST: Mutex<Collapse> = Mutex::new(Collapse { last: None, repeats: 0 });

/// Writes the "repeated N times" line a run of identical lines still owes. Call before reading
/// the log (the diagnostics file does), so the newest line says how often it happened.
pub fn flush() {
    let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((level, what)) = last.take() {
        emit(&level, &what);
    }
}

/// Writes one line, unless it repeats the one before (see [`Collapse`]).
fn line(level: &str, what: &str) {
    let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
    for (level, what) in last.push(level, what) {
        emit(&level, &what);
    }
}

/// Writes one line to the file. It also goes to stderr when it is a warning (a terminal run, or a
/// startup error before anything else is up, still shows it) or when there is no file to write.
fn emit(level: &str, what: &str) {
    let text = format_line(now(), level, what);
    let filed = DIR.get().is_some_and(|dir| append(dir, &text, LIMIT).is_ok());
    if level == "WARN" || !filed {
        eprint!("{text}");
    }
}

/// Seconds since the Unix epoch now; 0 if the clock is before it.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `<UTC time> <LEVEL> <what>\n`, with every control character of `what` made a space: a device
/// name chosen by someone else on the network must not forge a second line.
fn format_line(secs: u64, level: &str, what: &str) -> String {
    let what: String = what.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    format!("{} {level} {what}\n", utc(secs))
}

/// Appends `text` to `dir`'s log, first moving the file to the old name if `text` would push it
/// past `limit` bytes. The file is opened per line, so a rotation never leaves a handle on the old file.
fn append(dir: &Path, text: &str, limit: u64) -> std::io::Result<()> {
    let _held = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    let path = dir.join(FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() + text.len() as u64 > limit) {
        std::fs::rename(&path, dir.join(OLD))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}

/// Year, month, day, hour, minute and second (UTC) of `secs` since the epoch.
fn civil(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a calendar date (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day, rest / 3_600, rest % 3_600 / 60, rest % 60)
}

/// `secs` since the epoch as `2026-11-28T12:34:56Z`.
pub fn utc(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `secs` since the epoch as `20261128-123456` (UTC), for file names.
pub fn stamp(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// The current time as [`stamp`].
pub fn stamp_now() -> String {
    stamp(now())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh empty folder.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("overdesk-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn times_are_utc_including_leap_days_and_year_ends() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(utc(1_767_225_599), "2025-12-31T23:59:59Z");
        assert_eq!(stamp(1_000_000_000), "20010909-014640");
    }

    #[test]
    fn a_line_cannot_be_split_by_what_it_carries() {
        let line = format_line(0, "INFO", "peer \"A\r\nFAKE\" \u{1b}[31mred");
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n') && !line.contains('\r') && !line.contains('\u{1b}'));
        assert!(line.starts_with("1970-01-01T00:00:00Z INFO peer"));
    }

    #[test]
    fn the_file_rotates_at_the_limit_and_keeps_one_old_file() {
        let dir = temp("rotate");
        for n in 0..10 {
            append(&dir, &format!("line {n}\n"), 25).unwrap();
        }
        let new = std::fs::read_to_string(dir.join(FILE)).unwrap();
        let old = std::fs::read_to_string(dir.join(OLD)).unwrap();
        assert_eq!(new, "line 9\n");
        assert_eq!(old, "line 6\nline 7\nline 8\n");
        assert!(new.len() <= 25 && old.len() <= 25);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no third file");
    }

    #[test]
    fn identical_lines_in_a_row_are_written_once_then_counted() {
        let mut c = Collapse::default();
        let line = |l: &str, w: &str| (l.to_owned(), w.to_owned());
        assert_eq!(c.push("WARN", "down"), [line("WARN", "down")]);
        assert!(c.push("WARN", "down").is_empty());
        assert!(c.push("WARN", "down").is_empty());
        // Same text at another level, or other text, ends the run and reports it.
        assert_eq!(
            c.push("INFO", "down"),
            [line("INFO", "last line repeated 2 times"), line("INFO", "down")]
        );
        assert_eq!(c.push("INFO", "up"), [line("INFO", "up")], "no repeats, nothing owed");
        assert_eq!(c.take(), None);
        assert!(c.push("INFO", "up").is_empty());
        assert_eq!(c.take(), Some(line("INFO", "last line repeated 1 times")));
        assert_eq!(c.take(), None, "reported once");
    }

    #[test]
    fn a_missing_folder_is_an_error_not_a_panic() {
        assert!(append(Path::new("/nonexistent/overdesk"), "x\n", LIMIT).is_err());
    }

    #[test]
    fn a_peer_id_is_shortened_to_eight_hex_digits() {
        assert_eq!(short_id(&PeerId([0xab; 16])), "abababab");
    }
}
