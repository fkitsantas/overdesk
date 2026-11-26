// SPDX-License-Identifier: AGPL-3.0-only
//! The Free app: the library's window around the open controller.

fn main() -> iced::Result {
    overdesk_app::run(|config, platform| Box::pin(overdeskd::controller::spawn(config, platform)))
}
