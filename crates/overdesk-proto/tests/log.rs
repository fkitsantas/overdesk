// SPDX-License-Identifier: AGPL-3.0-only
//! The log end to end: `init`, then `info` and `warn` through the process-wide state. One test, in
//! its own process, because `init` is once per process.

use overdesk_proto::log;

#[test]
fn lines_reach_the_file_in_the_logs_folder_with_a_utc_time_and_a_level() {
    let store = std::env::temp_dir().join(format!("overdesk-log-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store);
    log::init(&store);
    log::info("started \u{1F680}");
    log::warn(format_args!("careful {}", 7));
    let text = std::fs::read_to_string(store.join("logs").join(log::FILE)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    for (line, rest) in lines.iter().zip(["INFO started \u{1F680}", "WARN careful 7"]) {
        let (time, level_and_text) = line.split_once(' ').unwrap();
        assert!(
            time.len() == 20 && time.ends_with('Z') && time.as_bytes()[10] == b'T',
            "{time}"
        );
        assert_eq!(level_and_text, rest);
    }
    assert_eq!(log::dir(), Some(store.join("logs").as_path()));
    // A second init changes nothing.
    log::init(&store.join("elsewhere"));
    assert_eq!(log::dir(), Some(store.join("logs").as_path()));
}
