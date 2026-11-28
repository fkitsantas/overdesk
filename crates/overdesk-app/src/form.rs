// SPDX-License-Identifier: AGPL-3.0-only
//! The window's own form state: the address to dial, and Activate Pro while it waits for the browser.

use std::net::{IpAddr, SocketAddr};

use iced::Task;
use overdeskd::Side;

use super::sys::{lan_ip, parse_address};
use super::{Edition, Message};

/// The "Enter an address instead" fallback and the choice of side that goes with every connect.
pub(super) struct AddressForm {
    /// What the user has typed into the address field.
    pub(super) address: String,
    /// The Left / Right choice for the other device's screen.
    pub(super) side: Side,
    /// Whether the fallback is expanded.
    pub(super) open: bool,
    /// This computer's address on the local network, read when the fallback opens.
    pub(super) lan_ip: Option<IpAddr>,
}

impl AddressForm {
    /// A closed form with nothing typed and the other screen on the right.
    pub(super) const fn new() -> Self {
        Self {
            address: String::new(),
            side: Side::Right,
            open: false,
            lan_ip: None,
        }
    }

    /// Expands or collapses the fallback.
    pub(super) fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.lan_ip = lan_ip(); // read when opened, not on every frame
        }
    }

    /// The address typed, if it is one that can be dialed.
    pub(super) fn target(&self) -> Option<SocketAddr> {
        parse_address(&self.address)
    }
}

/// The licence row of the official build.
#[derive(Default)]
pub(super) struct Activation {
    /// While waiting for the browser: the code the page shows, and the handle that stops the wait.
    pub(super) waiting: Option<(String, iced::task::Handle)>,
    /// Why the last activation failed, until the next attempt.
    pub(super) error: Option<String>,
    /// The licence email, once Pro was activated in this run (applies after a restart).
    pub(super) activated: Option<String>,
}

impl Activation {
    /// Starts Activate Pro with `edition`: opens the browser page and waits for the licence.
    pub(super) fn start(&mut self, edition: &Edition) -> Task<Message> {
        self.cancel();
        self.error = None;
        let started = (edition.activate)();
        if super::sys::open(&started.url).is_err() {
            self.error = Some(format!("Couldn't open the browser. Open {} to activate.", started.url));
        }
        let (task, handle) = Task::perform(started.done, Message::Activated).abortable();
        self.waiting = Some((started.code, handle.abort_on_drop()));
        task
    }

    /// Stops waiting (the browser page, if approved later, is simply never collected).
    pub(super) fn cancel(&mut self) {
        if let Some((_, handle)) = self.waiting.take() {
            handle.abort();
        }
    }

    /// The wait ended with the licensed email or why not.
    pub(super) fn finish(&mut self, result: Result<String, String>) {
        self.waiting = None;
        match result {
            Ok(email) => {
                self.activated = Some(email);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_activation_keeps_the_email_and_a_failed_one_the_reason() {
        let mut form = Activation::default();
        form.finish(Err("no Pro".into()));
        assert_eq!(
            (form.activated.as_deref(), form.error.as_deref()),
            (None, Some("no Pro"))
        );
        form.finish(Ok("k@example.org".into()));
        assert_eq!(form.activated.as_deref(), Some("k@example.org"));
        assert!(form.error.is_none() && form.waiting.is_none());
    }
}
