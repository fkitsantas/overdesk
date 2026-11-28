// SPDX-License-Identifier: AGPL-3.0-only
//! What the user has typed into the window's fields: the address to dial and the licence key.

use std::net::{IpAddr, SocketAddr};

use overdeskd::Side;

use super::Edition;
use super::sys::{lan_ip, parse_address};

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
    /// The licence key being typed.
    pub(super) key: String,
    /// Why the last activation failed, until the next attempt.
    pub(super) error: Option<String>,
    /// The licence email, once a key was activated in this run (applies after a restart).
    pub(super) activated: Option<String>,
}

impl Activation {
    /// Checks the typed key with `edition`.
    pub(super) fn activate(&mut self, edition: &Edition) {
        match (edition.activate)(self.key.trim()) {
            Ok(email) => {
                self.activated = Some(email);
                self.key.clear();
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
    fn a_good_key_clears_the_field_and_a_bad_one_keeps_it_with_the_reason() {
        let edition = |activate: fn(&str) -> Result<String, String>| Edition {
            licensed_to: None,
            activate,
        };
        let mut form = Activation {
            key: " k ".into(),
            ..Activation::default()
        };
        form.activate(&edition(|k| Err(format!("no {k}"))));
        assert_eq!((form.key.as_str(), form.error.as_deref()), (" k ", Some("no k")));
        form.activate(&edition(|k| Ok(format!("{k}@example.org"))));
        assert_eq!(form.activated.as_deref(), Some("k@example.org"));
        assert!(form.key.is_empty() && form.error.is_none());
    }
}
