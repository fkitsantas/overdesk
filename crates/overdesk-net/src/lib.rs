// SPDX-License-Identifier: AGPL-3.0-only
//! Overdesk networking: device identity, pairing, and the two QUIC lanes.
//! See `docs/ARCHITECTURE.md` §2 and §7.

#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))] // test names document themselves
pub mod pairing;
pub mod session;
pub mod store;
pub mod transport;
