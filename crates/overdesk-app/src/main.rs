// SPDX-License-Identifier: AGPL-3.0-only
//! The Free app: the library's window around the open controller.

fn main() -> iced::Result {
    // Before any window exists: Windows refuses the change afterwards.
    #[cfg(windows)]
    if let Some(w) = overdesk_platform::win::ffi::ensure_dpi_aware() {
        overdesk_proto::log::warn(w);
    }
    overdesk_app::run(|config, platform| Box::pin(overdeskd::controller::spawn(config, platform)))
}
