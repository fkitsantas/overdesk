// SPDX-License-Identifier: AGPL-3.0-only
//! Debug output that never shows what the user typed or copied.

use overdesk_core::engine::{Action, Event};
use overdesk_proto::Control;

/// What replaces typed or copied content in debug output.
const REDACTED: &str = "<redacted>";

/// `msg` with the clipboard text, typed text and key codes hidden, safe to print.
fn redact_control(msg: &Control) -> Control {
    match msg {
        Control::Clipboard { .. } => Control::Clipboard { text: REDACTED.into() },
        Control::KeyText { .. } => Control::KeyText {
            code: 0,
            text: REDACTED.into(),
        },
        Control::Key { pressed, .. } => Control::Key {
            code: 0,
            pressed: *pressed,
        },
        other => other.clone(),
    }
}

/// `event` with what the user typed or copied hidden, safe to print: `OVERDESK_DEBUG` output
/// ends up in logs and screenshots, and passwords go through keys.
pub(crate) fn redact_event(event: &Event) -> Event {
    match event {
        Event::LocalKey { pressed, .. } => Event::LocalKey {
            code: 0,
            pressed: *pressed,
        },
        Event::Peer { from, msg } => Event::Peer {
            from: *from,
            msg: redact_control(msg),
        },
        other => other.clone(),
    }
}

/// `action` with what the user typed or copied hidden, safe to print (see `redact_event`).
pub fn redact_action(action: &Action) -> Action {
    match action {
        Action::Send { to, msg } => Action::Send {
            to: *to,
            msg: redact_control(msg),
        },
        Action::InjectKey { pressed, .. } => Action::InjectKey {
            code: 0,
            pressed: *pressed,
        },
        Action::InjectText { .. } => Action::InjectText { text: REDACTED.into() },
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overdesk_proto::PeerId;

    #[test]
    fn debug_output_hides_what_was_typed_or_copied() {
        let secret = "hunter2";
        let peer = PeerId([1; 16]);
        let shown = format!(
            "{:?}{:?}{:?}{:?}{:?}{:?}",
            redact_event(&Event::Peer {
                from: peer,
                msg: Control::Clipboard { text: secret.into() }
            }),
            redact_event(&Event::Peer {
                from: peer,
                msg: Control::KeyText {
                    code: 4242,
                    text: secret.into()
                }
            }),
            redact_event(&Event::LocalKey {
                code: 4242,
                pressed: true
            }),
            redact_action(&Action::InjectText { text: secret.into() }),
            redact_action(&Action::InjectKey {
                code: 4242,
                pressed: true
            }),
            redact_action(&Action::Send {
                to: peer,
                msg: Control::Key {
                    code: 4242,
                    pressed: true
                }
            }),
        );
        assert!(!shown.contains(secret) && !shown.contains("4242"), "{shown}");
        assert!(shown.contains("pressed: true"), "{shown}");
    }
}
