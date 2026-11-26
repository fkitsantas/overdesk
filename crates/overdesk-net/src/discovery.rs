// SPDX-License-Identifier: AGPL-3.0-only
//! LAN discovery over multicast DNS (DNS-SD), so users never type IP addresses.
//!
//! Each device runs an [`Advertiser`] that publishes a [`SERVICE`] instance whose TXT
//! records carry its identity, and a [`Browser`] that reports the other devices it hears
//! as [`Change`] events. Everything read from the network is untrusted: [`parse`] validates
//! it and silently drops anything malformed.

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use overdesk_proto::{Os, PROTOCOL_VERSION, PeerId};
use std::{
    net::{IpAddr, SocketAddr},
    sync::mpsc,
};

/// DNS-SD service type under which Overdesk devices announce themselves.
pub const SERVICE: &str = "_overdesk._udp.local.";
/// Longest accepted device name, in characters; longer names are truncated.
const MAX_NAME_CHARS: usize = 64;
/// Length of a hex-encoded [`PeerId`] (16 bytes).
const ID_HEX_LEN: usize = 32;

/// Failure to start advertising or browsing.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The mDNS daemon or its registration failed (for example, no usable network interface).
    #[error("mDNS: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

/// A device seen on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nearby {
    /// The device's stable identity.
    pub id: PeerId,
    /// Human-readable device name, at most 64 characters, without control characters.
    pub name: String,
    /// Operating system the device runs.
    pub os: Os,
    /// Where to dial it: an IPv4 address when it has one, otherwise IPv6.
    pub address: SocketAddr,
}

/// A change in the set of nearby devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A device appeared or re-announced itself.
    Found(Nearby),
    /// A device went away.
    Lost(PeerId),
}

/// Lower-case hex of a [`PeerId`]; also used as the DNS-SD instance name.
fn id_hex(id: PeerId) -> String {
    id.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// Inverse of [`id_hex`]. Rejects anything that is not exactly 32 ASCII hex digits.
fn id_from_hex(s: &str) -> Option<PeerId> {
    if s.len() != ID_HEX_LEN || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (byte, pair) in out.iter_mut().zip(s.as_bytes().chunks(2)) {
        let digits = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(digits, 16).ok()?;
    }
    Some(PeerId(out))
}

/// TXT value for an [`Os`].
fn os_str(os: Os) -> &'static str {
    match os {
        Os::MacOs => "macos",
        Os::Windows => "windows",
        Os::Linux => "linux",
        Os::Android => "android",
    }
}

/// Parses the TXT `os` value; unknown values are rejected.
fn os_from_str(s: &str) -> Option<Os> {
    Some(match s {
        "macos" => Os::MacOs,
        "windows" => Os::Windows,
        "linux" => Os::Linux,
        "android" => Os::Android,
        _ => return None,
    })
}

/// Unicode characters that change how text displays without being visible: format
/// characters such as bidirectional overrides and zero-width joiners, the byte-order
/// mark, and line or paragraph separators. A device name carrying them could display as
/// a different name in the Nearby list, so they are removed.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'                    // soft hyphen
            | '\u{061C}'              // Arabic letter mark
            | '\u{180E}'              // Mongolian vowel separator
            | '\u{200B}'..='\u{200F}' // zero-width space/joiners, LRM, RLM
            | '\u{2028}'..='\u{202E}' // line/paragraph separators, bidi embeddings and overrides
            | '\u{2060}'..='\u{206F}' // word joiner, invisible operators, bidi isolates
            | '\u{FEFF}'              // byte-order mark / zero-width no-break space
            | '\u{FFF9}'..='\u{FFFB}' // interlinear annotation controls
    )
}

/// Validates one announcement. `txt` holds the TXT key/value pairs, `addrs` the resolved
/// addresses and `port` the service port.
///
/// Returns `None` when the id is not 32 hex digits, the OS is unknown, the protocol
/// version differs from [`PROTOCOL_VERSION`], the name is empty after sanitising, or there
/// is no address. Names longer than 64 characters are truncated (not rejected) and control
/// characters are dropped, so a hostile name cannot disturb the UI.
fn parse(txt: &[(&str, &str)], addrs: &[IpAddr], port: u16) -> Option<Nearby> {
    let get = |key: &str| txt.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
    let id = id_from_hex(get("id")?)?;
    let os = os_from_str(get("os")?)?;
    if get("v")?.parse::<u16>().ok()? != PROTOCOL_VERSION {
        return None;
    }
    let name: String = get("name")?
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .take(MAX_NAME_CHARS)
        .collect();
    if name.is_empty() {
        return None;
    }
    let ip = addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.first())?;
    Some(Nearby {
        id,
        name,
        os,
        address: SocketAddr::new(*ip, port),
    })
}

/// Publishes this device on the LAN until dropped.
pub struct Advertiser {
    /// The mDNS responder thread handle.
    daemon: ServiceDaemon,
    /// Full DNS name of the registered instance, needed to unregister it.
    fullname: String,
}

impl Advertiser {
    /// Registers this device as a [`SERVICE`] instance on `port`, with TXT records `id`,
    /// `name`, `os` and `v`. Addresses are taken from the host's interfaces automatically.
    pub fn start(id: PeerId, name: &str, os: Os, port: u16) -> Result<Self, DiscoveryError> {
        let hex = id_hex(id);
        let version = PROTOCOL_VERSION.to_string();
        let txt = [
            ("id", hex.as_str()),
            ("name", name),
            ("os", os_str(os)),
            ("v", version.as_str()),
        ];
        let info = ServiceInfo::new(SERVICE, &hex, &format!("{hex}.local."), "", port, &txt[..])?.enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        let daemon = ServiceDaemon::new()?;
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }
}

impl Drop for Advertiser {
    /// Withdraws the announcement and stops the responder; errors are ignored as the
    /// daemon may already be gone.
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Listens for other Overdesk devices until dropped.
pub struct Browser {
    /// The mDNS daemon; shutting it down ends the forwarding thread.
    daemon: ServiceDaemon,
}

impl Browser {
    /// Starts browsing for [`SERVICE`]. Devices other than `own_id` that pass validation
    /// are reported on the returned channel; the channel closes when the `Browser` drops.
    pub fn start(own_id: PeerId) -> Result<(Self, mpsc::Receiver<Change>), DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let events = daemon.browse(SERVICE)?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                let change = match event {
                    ServiceEvent::ServiceResolved(svc) => {
                        let txt: Vec<(&str, &str)> =
                            svc.txt_properties.iter().map(|p| (p.key(), p.val_str())).collect();
                        let addrs: Vec<IpAddr> = svc.addresses.iter().map(|a| a.to_ip_addr()).collect();
                        parse(&txt, &addrs, svc.port)
                            .filter(|n| n.id != own_id)
                            .map(Change::Found)
                    }
                    // The instance name is the hex id, so removal needs no cached state.
                    ServiceEvent::ServiceRemoved(_, fullname) => fullname
                        .split('.')
                        .next()
                        .and_then(id_from_hex)
                        .filter(|id| *id != own_id)
                        .map(Change::Lost),
                    _ => None,
                };
                if change.is_some_and(|c| tx.send(c).is_err()) {
                    break; // receiver dropped
                }
            }
        });
        Ok((Self { daemon }, rx))
    }
}

impl Drop for Browser {
    /// Stops the daemon, which closes the event stream and ends the forwarding thread.
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::{Ipv4Addr, Ipv6Addr},
        time::Duration,
    };

    const HEX: &str = "000102030405060708090a0b0c0d0e0f";
    const V4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 7));
    const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

    fn txt<'a>(id: &'a str, name: &'a str, os: &'a str, v: &'a str) -> Vec<(&'a str, &'a str)> {
        vec![("id", id), ("name", name), ("os", os), ("v", v)]
    }

    fn good() -> Vec<(&'static str, &'static str)> {
        txt(HEX, "Studio Mac", "macos", "1")
    }

    #[test]
    fn valid_input_parses() {
        let n = parse(&good(), &[V4], 4433).expect("valid");
        assert_eq!(n.id, PeerId(std::array::from_fn(|i| i as u8)));
        assert_eq!(n.name, "Studio Mac");
        assert_eq!(n.os, Os::MacOs);
        assert_eq!(n.address, SocketAddr::new(V4, 4433));
    }

    #[test]
    fn hex_round_trips() {
        let id = PeerId([0xab; 16]);
        assert_eq!(id_from_hex(&id_hex(id)), Some(id));
    }

    #[test]
    fn bad_ids_are_rejected() {
        for bad in [
            "",
            "00",
            &HEX[..31],
            &format!("{HEX}0"),
            "zz0102030405060708090a0b0c0d0e0f",
            "é0102030405060708090a0b0c0d0e0",
        ] {
            assert!(parse(&txt(bad, "n", "linux", "1"), &[V4], 1).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn overlong_name_is_truncated_to_64_chars() {
        let long = "é".repeat(200);
        let n = parse(&txt(HEX, &long, "linux", "1"), &[V4], 1).expect("truncated, not rejected");
        assert_eq!(n.name.chars().count(), 64);
    }

    #[test]
    fn control_chars_are_stripped_and_empty_names_rejected() {
        assert_eq!(
            parse(&txt(HEX, "a\nb\u{7}", "linux", "1"), &[V4], 1).unwrap().name,
            "ab"
        );
        assert!(parse(&txt(HEX, "", "linux", "1"), &[V4], 1).is_none());
        assert!(parse(&txt(HEX, "\n\t", "linux", "1"), &[V4], 1).is_none());
    }

    #[test]
    fn invisible_and_direction_changing_characters_are_stripped() {
        // "Work PC" with a right-to-left override, zero-width space and BOM hidden inside.
        let sneaky = "Work\u{202E}\u{200B} PC\u{FEFF}\u{2028}";
        assert_eq!(
            parse(&txt(HEX, sneaky, "linux", "1"), &[V4], 1).unwrap().name,
            "Work PC"
        );
        assert!(parse(&txt(HEX, "\u{200D}\u{2066}", "linux", "1"), &[V4], 1).is_none());
    }

    #[test]
    fn unknown_os_is_rejected() {
        assert!(parse(&txt(HEX, "n", "beos", "1"), &[V4], 1).is_none());
        assert!(parse(&txt(HEX, "n", "MacOS", "1"), &[V4], 1).is_none());
    }

    #[test]
    fn every_os_round_trips() {
        for os in [Os::MacOs, Os::Windows, Os::Linux, Os::Android] {
            assert_eq!(os_from_str(os_str(os)), Some(os));
        }
    }

    #[test]
    fn wrong_or_garbled_version_is_rejected() {
        assert!(parse(&txt(HEX, "n", "linux", "2"), &[V4], 1).is_none());
        assert!(parse(&txt(HEX, "n", "linux", "x"), &[V4], 1).is_none());
    }

    #[test]
    fn missing_keys_are_rejected() {
        for skip in 0..4 {
            let mut t = good();
            t.remove(skip);
            assert!(parse(&t, &[V4], 1).is_none(), "missing key {skip}");
        }
    }

    #[test]
    fn no_addresses_is_rejected() {
        assert!(parse(&good(), &[], 1).is_none());
    }

    #[test]
    fn ipv4_is_preferred_over_ipv6() {
        assert_eq!(parse(&good(), &[V6, V4], 9).unwrap().address.ip(), V4);
        assert_eq!(parse(&good(), &[V6], 9).unwrap().address.ip(), V6);
    }

    #[test]
    #[ignore = "needs multicast"]
    fn advertiser_is_found_by_browser() {
        let (a, b) = (PeerId([1; 16]), PeerId([2; 16]));
        let _adv = Advertiser::start(a, "Loopback Test", Os::Linux, 4433).expect("advertise");
        let (_browser, rx) = Browser::start(b).expect("browse");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left).expect("Found within 10 s") {
                Change::Found(n) => {
                    assert_eq!(
                        (n.id, n.name.as_str(), n.os, n.address.port()),
                        (a, "Loopback Test", Os::Linux, 4433)
                    );
                    break;
                }
                Change::Lost(_) => {}
            }
        }
    }
}
