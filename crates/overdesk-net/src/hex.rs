// SPDX-License-Identifier: AGPL-3.0-only
//! Lower-case hex for identifiers: the one encoding lives in `overdesk-proto`; this path stays
//! for the crates that already reach it through the network layer.

pub use overdesk_proto::hex::{hex, unhex, unhex_vec};
