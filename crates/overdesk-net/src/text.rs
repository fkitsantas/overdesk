// SPDX-License-Identifier: AGPL-3.0-only
//! Text that came from the network, made safe to show.

/// Longest device name kept, in characters; longer names are truncated.
const MAX_NAME_CHARS: usize = 64;

/// Removes control, invisible and direction-changing characters from a peer-supplied
/// device name and keeps at most 64 characters; discovery uses it for announced names too.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && !overdesk_proto::is_invisible(*c))
        .take(MAX_NAME_CHARS)
        .collect()
}
