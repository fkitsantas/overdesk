// SPDX-License-Identifier: AGPL-3.0-only
//! The Free app: the library's window around the open controller.
// A release build is a windowed app on Windows (no console window); a debug build keeps the
// console for stderr. Messages go to the log file either way.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> iced::Result {
    overdesk_proto::log::init(&overdeskd::store_dir());
    // Before any window exists: Windows refuses the change afterwards.
    #[cfg(windows)]
    if let Some(w) = overdesk_platform::win::ffi::ensure_dpi_aware() {
        overdesk_proto::log::warn(w);
    }
    let result = overdesk_app::run(|config, platform| Box::pin(overdeskd::controller::spawn(config, platform)));
    overdesk_proto::log::flush(); // a pending "repeated N times" is not lost at exit
    result
}
