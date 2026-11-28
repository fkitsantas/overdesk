// SPDX-License-Identifier: AGPL-3.0-only
//! Bug reports: the diagnostics file a person attaches, and the support link that opens prefilled.
//!
//! The file holds the same facts as the log and nothing the log may not (see
//! [`overdesk_proto::log`]): versions, OS, display, who is paired or nearby by short id and OS,
//! the connection state, then the newest log lines. The header has no device names, paths, keys,
//! PINs or codes; the log lines may name a peer by its device name, as the log does.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use overdesk_proto::log;
use overdeskd::controller::{AppState, Phase};

use crate::EditionReport;
use crate::sys;

/// The most bytes a diagnostics file holds, header included.
pub(crate) const MAX_BYTES: usize = 512 * 1024;
/// The support page the "Report a bug" item opens.
const SUPPORT: &str = "https://overdesk.org/support";
/// The most characters of any one value in the support link.
const MAX_VALUE: usize = 64;

/// What the UI thread knows that a report needs: the state's lines and the OS of each paired peer.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    /// What the build says about itself: version, edition and any extra header lines.
    pub(crate) edition: EditionReport,
    /// Header lines about the connection, the paired and nearby devices and the Desk.
    pub(crate) lines: Vec<String>,
    /// The OS ids of the paired peers whose OS is known, sorted, without repeats.
    pub(crate) peers: Vec<&'static str>,
}

/// Facts from the controller's state (`None` before it started) and the build's report.
pub(crate) fn facts(state: Option<&AppState>, edition: EditionReport) -> Facts {
    let Some(state) = state else {
        return Facts {
            edition,
            lines: vec!["connection: controller not running".into()],
            peers: Vec::new(),
        };
    };
    // A paired device's OS is known while it is nearby or connected.
    let os_of = |id| {
        let live = match &state.phase {
            Phase::Connected { peers, .. } => peers.iter().find(|p| p.id == id).map(|p| p.os),
            _ => None,
        };
        live.or_else(|| state.nearby.iter().find(|n| n.id == id).map(|n| n.os))
            .map(overdesk_proto::Os::id)
    };
    let connection = match &state.phase {
        Phase::Idle => "idle".to_owned(),
        Phase::Connecting { address } => format!("connecting to {address}"),
        Phase::Pairing { .. } => "pairing".into(),
        Phase::EnterPin { .. } => "waiting for a PIN".into(),
        Phase::Connected { peers, mode } => format!("connected to {} (cursor: {mode:?})", peers.len()),
        Phase::Lost { .. } => "lost the connection".into(),
        Phase::Error(why) => format!("error: {why}"),
    };
    let mut lines = vec![
        format!("connection: {connection}"),
        format!(
            "this computer: {}, listening on {}",
            state.os.id(),
            state.address.map_or_else(|| "nowhere".into(), |a| a.to_string())
        ),
    ];
    lines.extend(state.paired.iter().map(|p| {
        format!(
            "paired: {} {} {}",
            log::short_id(&p.id),
            os_of(p.id).unwrap_or("unknown"),
            if p.online { "online" } else { "offline" }
        )
    }));
    lines.extend(
        state
            .nearby
            .iter()
            .filter(|n| !n.paired)
            .map(|n| format!("nearby: {} {} at {}", log::short_id(&n.id), n.os.id(), n.address)),
    );
    lines.extend(state.desk.iter().map(|s| {
        format!(
            "screen: {} {}x{} at {},{}",
            if s.me {
                "this computer".into()
            } else {
                log::short_id(&s.id)
            },
            s.w,
            s.h,
            s.x,
            s.y
        )
    }));
    let peers = state
        .paired
        .iter()
        .filter_map(|p| os_of(p.id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Facts { edition, lines, peers }
}

/// The file's text: `header` as `# ` lines, then as many whole lines from the end of `logs` as fit
/// in `cap` bytes in all.
pub(crate) fn build(header: &[String], logs: &str, cap: usize) -> String {
    let mut out: String = header.iter().map(|h| format!("# {h}\n")).collect();
    out.push_str("# --- log, oldest first ---\n");
    let room = cap.saturating_sub(out.len());
    let mut used = 0;
    for line in logs.split_inclusive('\n').rev() {
        if used + line.len() > room {
            break;
        }
        used += line.len();
    }
    out.push_str(&logs[logs.len() - used..]);
    out
}

/// The old log then the current one, whole, however damaged (a bad byte becomes U+FFFD).
fn read_logs(dir: &Path) -> String {
    let read = |name| String::from_utf8_lossy(&std::fs::read(dir.join(name)).unwrap_or_default()).into_owned();
    let mut text = read(log::OLD);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text + &read(log::FILE)
}

/// The user's Downloads folder from the environment: `XDG_DOWNLOAD_DIR` when absolute, else
/// `Downloads` in the home folder (`USERPROFILE` on Windows, `HOME` elsewhere).
fn downloads_from(get: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    get("XDG_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| get(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| PathBuf::from(h).join("Downloads")))
}

/// `arm64` for what Rust calls `aarch64`; any other architecture as Rust names it (`x86_64`).
fn arch_id(rust: &str) -> &str {
    if rust == "aarch64" { "arm64" } else { rust }
}

/// `value` cut to [`MAX_VALUE`] characters and percent-encoded (everything but `A-Za-z0-9-._~`).
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.chars().take(MAX_VALUE).collect::<String>().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The OS ids joined with commas, cut between ids so none is half there.
fn peers_value(ids: &[&str]) -> String {
    let mut out = String::new();
    for id in ids {
        let add = id.len() + usize::from(!out.is_empty());
        if out.len() + add > MAX_VALUE {
            break;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(id);
    }
    out
}

/// The support page with what the user would otherwise be asked, each value encoded and at most
/// [`MAX_VALUE`] characters; empty ones are left out.
pub(crate) fn report_url(params: &[(&str, &str)]) -> String {
    let query: Vec<String> = params
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect();
    format!("{SUPPORT}?{}", query.join("&"))
}

/// A saved diagnostics file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Saved {
    /// The file's name.
    pub(crate) name: String,
    /// Where it is.
    pub(crate) path: PathBuf,
    /// Whether it is in the Downloads folder (else the store folder, as a fallback).
    pub(crate) in_downloads: bool,
    /// Whether the browser or file manager was asked to open and accepted.
    pub(crate) opened: bool,
    /// For a bug report, the link that was opened.
    pub(crate) url: Option<String>,
}

impl Saved {
    /// What the user reads once it is done.
    pub(crate) fn notice(&self) -> String {
        let place = if self.in_downloads {
            "Downloads".to_owned()
        } else {
            self.path.parent().map_or_else(String::new, |p| p.display().to_string())
        };
        let mut text = format!("Saved {} to {place}. Attach it to your report.", self.name);
        if !self.opened {
            text.push_str(" Overdesk couldn't open it for you.");
        }
        text
    }
}

/// Writes `text` as `name` in the first of `dirs` that takes it, noting whether that is Downloads.
fn write_first(dirs: &[(PathBuf, bool)], name: &str, text: &str) -> Result<(PathBuf, bool), String> {
    let mut last = String::from("no folder to save to");
    for (dir, downloads) in dirs {
        let path = dir.join(name);
        match std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, text)) {
            Ok(()) => return Ok((path, *downloads)),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// The file's name for the time `stamp` (see [`log::stamp`]).
fn file_name(stamp: &str) -> String {
    format!("overdesk-diagnostics-{stamp}.txt")
}

/// The whole file: the header (versions, system, then the connection facts and the edition's own
/// lines) and the newest of `logs`, at most [`MAX_BYTES`].
fn compose(facts: &Facts, osv: &str, displays: &str, vpn: bool, now: u64, logs: &str) -> String {
    let mut header = vec![
        format!("Overdesk diagnostics, {}", log::utc(now)),
        format!("app: {}", facts.edition.version),
        format!("edition: {}", edition_id(&facts.edition)),
        format!("os: {osv}"),
        format!("arch: {}", arch_id(std::env::consts::ARCH)),
        format!("displays: {displays}"),
        format!("vpn active: {vpn}"),
    ];
    header.extend(facts.lines.iter().cloned());
    header.extend(facts.edition.lines.iter().cloned());
    build(&header, logs, MAX_BYTES)
}

/// `pro` or `free`, as the support link and the header say it.
fn edition_id(e: &EditionReport) -> &'static str {
    if e.pro { "pro" } else { "free" }
}

/// The Downloads folder, if this computer has one already: the desktop's own answer first
/// (`xdg-user-dir` on Linux and the BSDs, where it is a config entry, not a variable).
fn downloads() -> Option<PathBuf> {
    #[cfg(free_unix)]
    let asked = std::process::Command::new("xdg-user-dir")
        .arg("DOWNLOAD")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| OsString::from(String::from_utf8_lossy(&o.stdout).trim()))
        // With none configured it answers the home folder, which is no place for a file.
        .filter(|p| std::env::var_os("HOME").as_ref() != Some(p));
    #[cfg(not(free_unix))]
    let asked: Option<OsString> = None;
    // A folder that does not exist is the wrong guess (a translated or moved Downloads), not one to create.
    downloads_from(|k| std::env::var_os(k).or_else(|| asked.clone().filter(|_| k == "XDG_DOWNLOAD_DIR")))
        .filter(|d| d.is_dir())
}

/// Saves the diagnostics file, then opens the support page (`report`) or shows the file in the
/// file manager. Blocking: it reads the OS version, the displays, the network and the log.
pub(crate) fn run(facts: Facts, store: &Path, report: bool) -> Result<Saved, String> {
    let osv = overdeskd::os_version();
    let displays = overdeskd::native::monitors()
        .iter()
        .map(|m| format!("{}x{}", m.w, m.h))
        .collect::<Vec<_>>()
        .join(", ");
    log::flush(); // a pending "repeated N times" belongs in what is read
    let logs = read_logs(&log::dir().map_or_else(|| store.join("logs"), Path::to_path_buf));
    let text = compose(&facts, &osv, &displays, overdeskd::vpn_active(), now(), &logs);
    let name = file_name(&log::stamp_now());
    let mut dirs: Vec<(PathBuf, bool)> = downloads().map(|d| (d, true)).into_iter().collect();
    dirs.push((store.to_path_buf(), false));
    let (path, in_downloads) = write_first(&dirs, &name, &text)?;
    log::info(format_args!("diagnostics saved as {name} ({} bytes)", text.len()));
    let (opened, url) = if report {
        let url = report_url(&[
            ("from", "app"),
            ("app", facts.edition.version),
            ("edition", edition_id(&facts.edition)),
            ("os", std::env::consts::OS),
            ("osv", &osv),
            ("arch", arch_id(std::env::consts::ARCH)),
            ("peers", &peers_value(&facts.peers)),
            ("diag", &name),
        ]);
        (sys::open(&url).is_ok(), Some(url))
    } else {
        (sys::reveal(&path).is_ok(), None)
    };
    Ok(Saved {
        name,
        path,
        in_downloads,
        opened,
        url,
    })
}

/// Seconds since the Unix epoch now.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use overdesk_proto::{Os, PeerId};
    use overdeskd::controller::{DeviceKind, NearbyDevice, PairedDevice};

    fn report(lines: &[&str]) -> EditionReport {
        EditionReport {
            pro: false,
            version: "0.1.0",
            lines: lines.iter().map(|l| (*l).to_owned()).collect(),
        }
    }

    #[test]
    fn the_newest_whole_lines_fit_under_the_cap_after_the_header() {
        let header = vec!["app: 1".to_owned()];
        let logs = "one\ntwo\nthree\nfour\n";
        let full = build(&header, logs, 10_000);
        assert!(full.starts_with("# app: 1\n"));
        assert!(full.ends_with("one\ntwo\nthree\nfour\n"));
        let base = build(&header, "", 10_000).len();
        // Room for exactly the last two lines ("three\nfour\n" is 11 bytes) and a byte to spare.
        let cut = build(&header, logs, base + 12);
        assert!(cut.ends_with("three\nfour\n") && !cut.contains("two"), "{cut}");
        assert!(cut.len() <= base + 12);
        // Not even one line fits: the header stands alone, never half a line.
        assert_eq!(build(&header, logs, base + 3), build(&header, "", 10_000));
    }

    #[test]
    fn a_big_log_is_cut_to_the_cap_in_bytes_on_a_line_boundary() {
        let logs: String = (0..100_000).map(|n| format!("line {n} é✓\n")).collect();
        let text = build(&["x".to_owned()], &logs, MAX_BYTES);
        assert!(text.len() <= MAX_BYTES && text.len() > MAX_BYTES - 64);
        assert!(text.ends_with("line 99999 é✓\n"));
        assert!(text.lines().skip(2).all(|l| l.starts_with("line ")));
    }

    #[test]
    fn the_logs_are_the_old_file_then_the_new_one_with_bad_bytes_replaced() {
        let dir = std::env::temp_dir().join(format!("overdesk-diag-logs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(log::OLD), b"old no newline").unwrap();
        std::fs::write(dir.join(log::FILE), b"new \xff\n").unwrap();
        assert_eq!(read_logs(&dir), "old no newline\nnew \u{fffd}\n");
        assert_eq!(read_logs(&dir.join("missing")), "");
    }

    #[test]
    fn the_support_link_is_exactly_the_contract() {
        let url = report_url(&[
            ("from", "app"),
            ("app", "0.1.0"),
            ("edition", "free"),
            ("os", "macos"),
            ("osv", "macOS 15.2"),
            ("arch", "arm64"),
            ("peers", &peers_value(&["windows", "linux"])),
            ("diag", "overdesk-diagnostics-20261128-123456.txt"),
        ]);
        assert_eq!(
            url,
            "https://overdesk.org/support?from=app&app=0.1.0&edition=free&os=macos&osv=macOS%2015.2\
             &arch=arm64&peers=windows%2Clinux&diag=overdesk-diagnostics-20261128-123456.txt"
        );
    }

    #[test]
    fn values_are_encoded_cut_to_64_characters_and_unknown_ones_left_out() {
        let long = "é".repeat(100);
        let url = report_url(&[("osv", &long), ("peers", ""), ("diag", "a&b=c#d")]);
        assert_eq!(url, format!("{SUPPORT}?osv={}&diag=a%26b%3Dc%23d", "%C3%A9".repeat(64)));
        assert_eq!(report_url(&[]), format!("{SUPPORT}?"));
        let many = [
            "macos", "windows", "freebsd", "openbsd", "android", "linux", "macos", "windows", "linux",
        ];
        let peers = peers_value(&many);
        assert!(
            peers.len() <= 64 && !peers.ends_with(',') && peers.starts_with("macos,windows"),
            "{peers}"
        );
        assert!(many.join(",").starts_with(&peers));
    }

    #[test]
    fn the_architecture_is_arm64_or_as_rust_names_it() {
        assert_eq!((arch_id("aarch64"), arch_id("x86_64")), ("arm64", "x86_64"));
    }

    #[test]
    fn downloads_honours_xdg_when_absolute_else_the_home_folder() {
        let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
        };
        let xdg = if cfg!(windows) { r"C:\dl" } else { "/dl" };
        assert_eq!(
            downloads_from(env(Box::leak(Box::new([("XDG_DOWNLOAD_DIR", xdg), (home, "/h")])))),
            Some(PathBuf::from(xdg))
        );
        let relative = downloads_from(env(Box::leak(Box::new([("XDG_DOWNLOAD_DIR", "rel"), (home, "/h")]))));
        assert_eq!(relative, Some(PathBuf::from("/h").join("Downloads")));
        assert_eq!(downloads_from(env(&[])), None);
    }

    #[test]
    fn the_file_goes_to_the_first_folder_that_takes_it() {
        let base = std::env::temp_dir().join(format!("overdesk-diag-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let blocked = base.join("file");
        std::fs::write(&blocked, b"x").unwrap(); // a file cannot be a folder
        let (path, downloads) = write_first(
            &[(blocked.join("dl"), true), (base.join("store"), false)],
            "d.txt",
            "hi",
        )
        .unwrap();
        assert!(!downloads);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hi");
        assert!(write_first(&[(blocked.join("dl"), true)], "d.txt", "hi").is_err());
    }

    #[test]
    fn the_notice_names_downloads_only_when_the_file_is_there() {
        let saved = |in_downloads, opened| Saved {
            name: "f.txt".into(),
            path: PathBuf::from("/store/f.txt"),
            in_downloads,
            opened,
            url: None,
        };
        assert_eq!(
            saved(true, true).notice(),
            "Saved f.txt to Downloads. Attach it to your report."
        );
        assert!(saved(false, true).notice().starts_with("Saved f.txt to /store."));
        assert!(saved(true, false).notice().ends_with("couldn't open it for you."));
    }

    #[test]
    fn the_header_has_peers_by_short_id_and_os_and_never_a_code_a_pin_or_a_name() {
        let id = PeerId([0xab; 16]);
        let state = AppState {
            phase: Phase::Pairing {
                peer: "10.0.0.2".into(),
                code: 123_456,
            },
            paired: vec![PairedDevice {
                id,
                name: "Ada's private laptop".into(),
                kind: DeviceKind::Computer,
                online: true,
            }],
            nearby: vec![NearbyDevice {
                kind: DeviceKind::Computer,
                name: "Ada's private laptop".into(),
                os: Os::Windows,
                address: "10.0.0.2:24801".parse().unwrap(),
                id,
                paired: true,
            }],
            ..AppState::new("Ada's Mac".into(), Os::MacOs)
        };
        let f = facts(Some(&state), report(&[]));
        let text = f.lines.join("\n");
        assert!(text.contains("paired: abababab windows online"), "{text}");
        assert!(text.contains("connection: pairing"));
        assert_eq!(f.peers, ["windows"]);
        for secret in ["123456", "Ada"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        let pin = AppState {
            phase: Phase::EnterPin {
                name: "Living room TV".into(),
            },
            ..state
        };
        assert!(!facts(Some(&pin), report(&[])).lines.join("\n").contains("TV"));
    }

    #[test]
    fn the_file_has_its_header_lines_then_the_log_within_the_cap() {
        let facts = Facts {
            edition: EditionReport {
                pro: true,
                version: "9.9.9",
                lines: vec!["licence: Pro".into()],
            },
            lines: vec!["connection: idle".into()],
            peers: vec![],
        };
        let logs: String = (0..60_000)
            .map(|n| format!("2026-01-01T00:00:00Z INFO line {n}\n"))
            .collect();
        let text = compose(&facts, "macOS 15.2", "1440x900, 1920x1080", false, 1_000_000_000, &logs);
        let head: Vec<&str> = text.lines().take(9).collect();
        assert_eq!(
            head,
            [
                "# Overdesk diagnostics, 2001-09-09T01:46:40Z",
                "# app: 9.9.9",
                "# edition: pro",
                "# os: macOS 15.2",
                &format!("# arch: {}", arch_id(std::env::consts::ARCH)),
                "# displays: 1440x900, 1920x1080",
                "# vpn active: false",
                "# connection: idle",
                "# licence: Pro",
            ]
        );
        assert!(text.len() <= MAX_BYTES && text.ends_with("INFO line 59999\n"));
        assert_eq!(file_name("20010909-014640"), "overdesk-diagnostics-20010909-014640.txt");
    }

    #[test]
    fn without_a_controller_the_header_says_so() {
        let f = facts(None, report(&[]));
        assert_eq!(f.lines, ["connection: controller not running"]);
        assert!(f.peers.is_empty());
    }
}
