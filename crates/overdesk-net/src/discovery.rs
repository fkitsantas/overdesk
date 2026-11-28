// SPDX-License-Identifier: AGPL-3.0-only
//! LAN discovery over multicast DNS (DNS-SD), so users never type IP addresses.
//!
//! Each device runs an [`Advertiser`] that publishes a [`SERVICE`] instance whose TXT
//! records carry its identity, and a [`Browser`] that reports the other devices it hears
//! as [`Change`] events. Everything read from the network is untrusted: `parse` validates
//! it and silently drops anything malformed.

use mdns_sd::{DaemonEvent, IfKind, IfPredicate, ServiceDaemon, ServiceEvent, ServiceInfo};
use overdesk_proto::{Os, PROTOCOL_VERSION, PeerId};
use std::{
    net::{IpAddr, SocketAddr},
    sync::mpsc,
};

/// DNS-SD service type under which Overdesk devices announce themselves.
pub const SERVICE: &str = "_overdesk._udp.local.";
/// Events buffered between the mDNS thread and the consumer; more are dropped.
const EVENT_QUEUE: usize = 256;

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
    /// Where to dial it: an address on a subnet of one of our own non-tunnel interfaces if it
    /// has one, else a routable IPv4 address, then routable IPv6, then link-local ones.
    pub address: SocketAddr,
    /// Whether it announces Overdesk Pro (TXT `pro=1`). Unauthenticated, like the rest of the
    /// announcement: it only chooses how the Desk is shared, never whom to trust.
    pub pro: bool,
}

/// A change in the set of nearby devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A device appeared or re-announced itself.
    Found(Nearby),
    /// A device went away.
    Lost(PeerId),
    /// An address of this computer was added or removed (Wi-Fi joined, cable plugged, VPN
    /// switched): worth redialing at once. Tunnel interfaces never raise it.
    NetworkChanged,
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

/// Whether `ip` is link-local (169.254/16, fe80::/10): often the wrong interface, and an
/// IPv6 one is unusable without a scope id.
fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_link_local(),
        IpAddr::V6(a) => a.segments()[0] & 0xffc0 == 0xfe80,
    }
}

/// Name prefixes of tunnel interfaces: macOS `utun`, Linux `tun`/`tap`/`wg`/`ppp`/`ipsec`/`zt`
/// (ZeroTier), GlobalProtect `gpd`, and the Windows friendly names of the common VPN adapters.
const TUNNEL_PREFIXES: [&str; 12] = [
    "utun",
    "tun",
    "tap",
    "wg",
    "ppp",
    "ipsec",
    "tailscale",
    "zt",
    "gpd",
    "wintun",
    "nordlynx",
    "pangp",
];

/// Whether an interface called `name` looks like a VPN tunnel: its address is not on the LAN.
fn tunnel_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    TUNNEL_PREFIXES.iter().any(|p| n.starts_with(p)) || ["vpn", "wireguard", "anyconnect"].iter().any(|k| n.contains(k))
}

/// Windows `IfType` of a PPP adapter (dial-up and many VPN clients).
#[cfg(any(windows, test))]
const IF_TYPE_PPP: u32 = 23;
/// Windows `IfType` of a proprietary virtual adapter.
#[cfg(any(windows, test))]
const IF_TYPE_PROP_VIRTUAL: u32 = 53;
/// Windows `IfType` of a tunnel adapter.
#[cfg(any(windows, test))]
const IF_TYPE_TUNNEL: u32 = 131;

/// Whether a Windows adapter of `if_type` and `description` is a VPN tunnel: a PPP, tunnel or
/// virtual type, or a description naming a known VPN client or driver.
#[cfg(any(windows, test))]
fn tunnel_adapter(if_type: u32, description: &str) -> bool {
    const KEYWORDS: [&str; 12] = [
        "wireguard",
        "wintun",
        "tap-windows",
        "openvpn",
        "anyconnect",
        "globalprotect",
        "pangp",
        "fortinet",
        "nordlynx",
        "tailscale",
        "zerotier",
        "vpn",
    ];
    let d = description.to_ascii_lowercase();
    matches!(if_type, IF_TYPE_PPP | IF_TYPE_PROP_VIRTUAL | IF_TYPE_TUNNEL) || KEYWORDS.iter().any(|k| d.contains(k))
}

/// Whether the Windows adapter with interface `index` is a tunnel by its type or description,
/// which the user cannot rename the way the friendly name can be; `false` if it cannot be read.
#[cfg(windows)]
#[allow(unsafe_code)] // one IP Helper call and a walk of the list it fills; invariants stated below
fn windows_tunnel(index: u32) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;
    let flags = GAA_FLAG_SKIP_UNICAST | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut bytes: u32 = 16 * 1024;
    // The list can grow between the size query and the fill, so ask again a few times.
    for _ in 0..4 {
        // u64 units: the buffer holds structs that need 8-byte alignment.
        let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
        let head = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: `head` points at `bytes` writable bytes, which `bytes` states; no reserved argument is used.
        let rc = unsafe { GetAdaptersAddresses(u32::from(AF_UNSPEC), flags, std::ptr::null(), head, &mut bytes) };
        if rc == ERROR_BUFFER_OVERFLOW {
            continue; // `bytes` now holds the size needed
        }
        if rc != ERROR_SUCCESS {
            return false;
        }
        let mut next = head.cast_const();
        while !next.is_null() {
            // SAFETY: on success the buffer holds a valid list: each `Next` is null or points inside it,
            // and each `Description` is a NUL-terminated UTF-16 string inside it.
            let a = unsafe { &*next };
            let ids = [unsafe { a.Anonymous1.Anonymous.IfIndex }, a.Ipv6IfIndex];
            if ids.contains(&index) {
                let mut len = 0;
                // SAFETY: as above; the string ends at its NUL.
                while !a.Description.is_null() && unsafe { *a.Description.add(len) } != 0 {
                    len += 1;
                }
                let text = if a.Description.is_null() {
                    String::new()
                } else {
                    // SAFETY: `len` units were just read from this string.
                    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(a.Description, len) })
                };
                return tunnel_adapter(a.IfType, &text);
            }
            next = a.Next.cast_const();
        }
        return false;
    }
    false
}

/// Whether `i` is a tunnel: the OS says point-to-point (macOS/Linux flag, Windows PPP and tunnel
/// types), else by name, and on Windows by the adapter's type and description too.
///
/// shortcut: on Windows each interface asks the IP Helper API afresh (a few milliseconds, only
/// when interfaces are listed); upgrade to a cache if profiling shows it. Linux and macOS have
/// only the name and the point-to-point flag, so a renamed WireGuard or Tailscale interface
/// there is missed (the same-subnet rule in [`on_our_lan`] still avoids dialing it).
fn is_tunnel(i: &if_addrs::Interface) -> bool {
    #[cfg(windows)]
    if i.index.is_some_and(windows_tunnel) {
        return true;
    }
    i.is_p2p() || tunnel_name(&i.name)
}

/// Keeps tunnels out of `daemon`: not announced on, not listened on, and so not reported as a
/// network change. A selection, so a tunnel that appears later is excluded too.
fn without_tunnels(daemon: &ServiceDaemon) -> Result<(), DiscoveryError> {
    daemon.disable_interface(IfKind::Predicate(IfPredicate::new(is_tunnel)))?;
    Ok(())
}

/// An address and prefix length of one of our own LAN interfaces.
type Subnet = (IpAddr, u8);

/// Our own non-tunnel interface subnets right now; empty when they cannot be read.
fn own_subnets() -> Vec<Subnet> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .iter()
        .filter(|i| i.is_oper_up() && !i.is_loopback() && !is_tunnel(i))
        .map(|i| match &i.addr {
            if_addrs::IfAddr::V4(a) => (IpAddr::V4(a.ip), a.prefixlen),
            if_addrs::IfAddr::V6(a) => (IpAddr::V6(a.ip), a.prefixlen),
        })
        .collect()
}

/// Whether `a` and `b` share their first `prefix` bits (same family only; IPv6 link-local never
/// counts, as every link has the same fe80::/64).
fn same_subnet(a: IpAddr, (b, prefix): Subnet) -> bool {
    let bits = |ip: IpAddr| match ip {
        IpAddr::V4(v) => (u128::from(u32::from(v)), 32u8),
        IpAddr::V6(v) => (u128::from(v), 128u8),
    };
    let ((x, width), (y, other)) = (bits(a), bits(b));
    let prefix = prefix.min(width);
    width == other && !is_link_local(&a) && (x ^ y).checked_shr(u32::from(width - prefix)).unwrap_or(0) == 0
}

/// Whether `a` is a global unicast IPv6 address (2000::/3): not link-local, ULA or loopback.
fn is_global_v6(a: &std::net::Ipv6Addr) -> bool {
    a.segments()[0] & 0xe000 == 0x2000
}

/// Whether an interface with these properties is a live VPN: a tunnel that is up and holds a
/// routable-looking IPv4 address or a global IPv6 one (an IPv6-only VPN). Why not link-local or
/// ULA: macOS always has idle utun0..utun7 with only fe80 addresses, and a real VPN tunnel gets
/// an IPv4 (only the VPN's utun8 had `10.1.0.4` here).
fn is_vpn(tunnel: bool, up: bool, addrs: &[IpAddr]) -> bool {
    tunnel
        && up
        && addrs.iter().any(|a| match a {
            IpAddr::V4(v) => !v.is_loopback() && !v.is_link_local(),
            IpAddr::V6(v) => is_global_v6(v),
        })
}

/// Whether a VPN tunnel is up on this computer now; `false` when the interfaces cannot be read.
pub fn vpn_active() -> bool {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .iter()
        .any(|i| is_vpn(is_tunnel(i), i.is_oper_up(), &[i.ip()]))
}

/// The addresses of `addrs` that are on a subnet of `own` (a tunnel address never is), or all of
/// them when none is: the announcing device may be reached on our LAN only by one of them.
fn on_our_lan(addrs: &[IpAddr], own: &[Subnet]) -> Vec<IpAddr> {
    let near: Vec<IpAddr> = addrs
        .iter()
        .copied()
        .filter(|a| own.iter().any(|s| same_subnet(*a, *s)))
        .collect();
    if near.is_empty() { addrs.to_vec() } else { near }
}

/// The address to dial: routable IPv4, then routable IPv6, then link-local IPv4, then
/// link-local IPv6. `None` for an empty list.
///
/// shortcut: one address per device; returning all candidates needs a new `Nearby` field.
fn best_address(addrs: &[IpAddr]) -> Option<IpAddr> {
    addrs.iter().min_by_key(|a| (is_link_local(a), a.is_ipv6())).copied()
}

/// Longest device name advertised, in bytes: a TXT `name=` entry must fit in 255 bytes.
const MAX_ADVERTISED_NAME_BYTES: usize = 240;

/// `name` cut to at most [`MAX_ADVERTISED_NAME_BYTES`] bytes on a character boundary.
fn clamp_name(name: &str) -> &str {
    let mut end = name.len().min(MAX_ADVERTISED_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
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
    let id = PeerId::from_hex(get("id")?)?;
    let os = os_from_str(get("os")?)?;
    if get("v")?.parse::<u16>().ok()? != PROTOCOL_VERSION {
        return None;
    }
    let name = crate::text::sanitize_name(get("name")?);
    if name.is_empty() {
        return None;
    }
    let ip = best_address(addrs)?;
    Some(Nearby {
        id,
        name,
        os,
        address: SocketAddr::new(ip, port),
        pro: get("pro") == Some("1"),
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
        Self::start_with(id, name, os, port, &[])
    }

    /// Like [`Advertiser::start`], with `extra` TXT records appended (Overdesk Pro adds `pro=1`).
    pub fn start_with(
        id: PeerId,
        name: &str,
        os: Os,
        port: u16,
        extra: &[(&str, &str)],
    ) -> Result<Self, DiscoveryError> {
        let hex = id.to_hex();
        let version = PROTOCOL_VERSION.to_string();
        let mut txt = vec![
            ("id", hex.as_str()),
            ("name", clamp_name(name)),
            ("os", os_str(os)),
            ("v", version.as_str()),
        ];
        txt.extend_from_slice(extra);
        let info = ServiceInfo::new(SERVICE, &hex, &format!("{hex}.local."), "", port, &txt[..])?.enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        let daemon = ServiceDaemon::new()?;
        if let Err(e) = without_tunnels(&daemon).and_then(|()| daemon.register(info).map_err(Into::into)) {
            let _ = daemon.shutdown(); // else the daemon thread outlives the failed start
            return Err(e);
        }
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
        let started = without_tunnels(&daemon).and_then(|()| {
            let events = daemon.browse(SERVICE)?;
            Ok((events, daemon.monitor()?))
        });
        let (events, monitor) = match started {
            Ok(e) => e,
            Err(e) => {
                let _ = daemon.shutdown();
                return Err(e);
            }
        };
        // Bounded: a flood of announcements must not grow memory while the UI is slow.
        let (tx, rx) = mpsc::sync_channel(EVENT_QUEUE);
        // The daemon rescans interfaces itself (every 5 s); this blocks, so it costs no polling.
        // It ends when the daemon shuts down and closes `monitor`.
        let net_tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(event) = monitor.recv() {
                if matches!(event, DaemonEvent::IpAdd(_) | DaemonEvent::IpDel(_))
                    && !forward(&net_tx, Change::NetworkChanged)
                {
                    break; // receiver dropped
                }
            }
        });
        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                let change = match event {
                    ServiceEvent::ServiceResolved(svc) => {
                        let txt: Vec<(&str, &str)> =
                            svc.txt_properties.iter().map(|p| (p.key(), p.val_str())).collect();
                        let addrs: Vec<IpAddr> = svc.addresses.iter().map(|a| a.to_ip_addr()).collect();
                        parse(&txt, &on_our_lan(&addrs, &own_subnets()), svc.port)
                            .filter(|n| n.id != own_id)
                            .map(Change::Found)
                    }
                    // The instance name is the hex id, so removal needs no cached state.
                    ServiceEvent::ServiceRemoved(_, fullname) => fullname
                        .split('.')
                        .next()
                        .and_then(PeerId::from_hex)
                        .filter(|id| *id != own_id)
                        .map(Change::Lost),
                    _ => None,
                };
                if change.is_some_and(|c| !forward(&tx, c)) {
                    break; // receiver dropped
                }
            }
        });
        Ok((Self { daemon }, rx))
    }
}

/// Passes `change` on; `false` once the receiver is gone.
fn forward(tx: &mpsc::SyncSender<Change>, change: Change) -> bool {
    match change {
        // A full queue drops a sighting; the next announcement refreshes the entry. A dropped
        // network change leaves others queued, which the consumer answers the same way.
        Change::Found(_) | Change::NetworkChanged => {
            !matches!(tx.try_send(change), Err(mpsc::TrySendError::Disconnected(_)))
        }
        // A removal is announced once, so it waits for room instead of being lost.
        Change::Lost(_) => tx.send(change).is_ok(),
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
    fn a_full_queue_drops_a_sighting_but_never_a_removal() {
        let (tx, rx) = mpsc::sync_channel(1);
        let found = Change::Found(parse(&good(), &[V4], 4433).unwrap());
        assert!(forward(&tx, found.clone()));
        assert!(forward(&tx, found.clone()), "dropped, not an error");
        let lost = Change::Lost(PeerId([1; 16]));
        let sender = std::thread::spawn({
            let lost = lost.clone();
            move || forward(&tx, lost)
        });
        assert_eq!(rx.recv().unwrap(), found);
        assert_eq!(rx.recv().unwrap(), lost, "the removal waited for room");
        assert!(sender.join().unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        assert!(!forward(&tx, found), "receiver gone");
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
    fn routable_beats_link_local_but_link_local_beats_nothing() {
        let ll4 = IpAddr::V4(Ipv4Addr::new(169, 254, 3, 4));
        let ll6 = IpAddr::V6("fe80::1".parse().unwrap());
        assert_eq!(parse(&good(), &[ll4, V4], 9).unwrap().address.ip(), V4);
        assert_eq!(parse(&good(), &[ll4, V6], 9).unwrap().address.ip(), V6);
        assert_eq!(parse(&good(), &[ll6, ll4], 9).unwrap().address.ip(), ll4);
        assert_eq!(parse(&good(), &[ll6], 9).unwrap().address.ip(), ll6);
    }

    #[test]
    fn advertised_name_is_clamped_on_a_char_boundary() {
        assert_eq!(clamp_name("short"), "short");
        let long = "é".repeat(200); // 400 bytes
        let c = clamp_name(&long);
        assert!(c.len() <= MAX_ADVERTISED_NAME_BYTES && c.len() >= MAX_ADVERTISED_NAME_BYTES - 1);
        assert!(c.chars().all(|ch| ch == 'é'));
        assert_eq!(clamp_name(&"x".repeat(300)).len(), MAX_ADVERTISED_NAME_BYTES);
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
                Change::Lost(_) | Change::NetworkChanged => {}
            }
        }
    }

    fn lan() -> Vec<Subnet> {
        vec![
            (IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 24),
            (IpAddr::V6("2001:db8:1::5".parse().unwrap()), 64),
        ]
    }

    #[test]
    fn a_tunnel_address_loses_to_one_on_our_lan() {
        let tunnel = IpAddr::V4(Ipv4Addr::new(10, 8, 0, 3));
        assert_eq!(on_our_lan(&[tunnel, V4], &lan()), vec![V4]);
        assert_eq!(
            parse(&good(), &on_our_lan(&[tunnel, V4], &lan()), 9)
                .unwrap()
                .address
                .ip(),
            V4
        );
    }

    #[test]
    fn without_a_shared_subnet_every_address_stays_for_the_usual_order() {
        let (a, b) = (
            IpAddr::V4(Ipv4Addr::new(10, 8, 0, 3)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9)),
        );
        assert_eq!(on_our_lan(&[a, b], &lan()), vec![a, b]);
        assert_eq!(on_our_lan(&[a, b], &[]), vec![a, b], "interfaces unreadable");
        assert_eq!(on_our_lan(&[V6, a], &lan()), vec![V6, a]);
    }

    #[test]
    fn ipv6_prefix_matches_but_link_local_and_other_families_never_do() {
        let near = IpAddr::V6("2001:db8:1::99".parse().unwrap());
        let far = IpAddr::V6("2001:db8:2::99".parse().unwrap());
        let ll = IpAddr::V6("fe80::1".parse().unwrap());
        assert_eq!(on_our_lan(&[far, near], &lan()), vec![near]);
        assert_eq!(
            on_our_lan(&[far, ll], &[(ll, 64)]),
            vec![far, ll],
            "fe80 is on every link"
        );
        assert!(!same_subnet(near, (IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)));
        assert!(same_subnet(V4, (V4, 32)) && same_subnet(V4, (V4, 0)));
        // A /0 on IPv6 shifts by the whole width, which must not panic.
        assert!(same_subnet(near, (far, 0)));
    }

    #[test]
    fn a_vpn_is_an_up_tunnel_with_a_real_ipv4_address() {
        let v4 = IpAddr::V4(Ipv4Addr::new(10, 1, 0, 4));
        let ll4 = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1));
        let ll6 = IpAddr::V6("fe80::1".parse().unwrap());
        assert!(is_vpn(true, true, &[ll6, v4]));
        assert!(!is_vpn(true, true, &[ll6, ll4]), "idle utun");
        assert!(!is_vpn(true, true, &[IpAddr::V4(Ipv4Addr::LOCALHOST)]));
        assert!(!is_vpn(true, true, &[]));
        let global = IpAddr::V6("2001:db8::5".parse().unwrap());
        assert!(is_vpn(true, true, &[ll6, global]), "an IPv6-only VPN");
        assert!(!is_vpn(true, true, &[IpAddr::V6("fd00::5".parse().unwrap())]), "ULA");
        assert!(!is_vpn(true, true, &[IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)]));
        assert!(!is_vpn(true, false, &[global]), "down");
        assert!(!is_vpn(true, false, &[v4]), "down");
        assert!(!is_vpn(false, true, &[v4]), "not a tunnel");
    }

    #[test]
    fn same_subnet_lan_beats_a_routable_tunnel_even_against_ipv4_preference() {
        let tunnel = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9));
        let v6 = IpAddr::V6("2001:db8:1::99".parse().unwrap());
        let picked = parse(&good(), &on_our_lan(&[tunnel, v6], &lan()), 9).unwrap();
        assert_eq!(picked.address.ip(), v6);
    }

    #[test]
    fn vpn_adapters_are_recognised_by_name() {
        for t in [
            "utun4",
            "tun0",
            "tap0",
            "wg0",
            "ppp0",
            "ipsec0",
            "tailscale0",
            "zt5u4",
            "gpd0",
            "WireGuard Tunnel",
            "ProtonVPN",
            "NordLynx",
        ] {
            assert!(tunnel_name(t), "{t}");
        }
        for l in [
            "en0", "eth0", "wlan0", "wlp3s0", "Ethernet", "Wi-Fi", "bridge0", "docker0",
        ] {
            assert!(!tunnel_name(l), "{l}");
        }
    }

    #[test]
    fn windows_adapters_are_tunnels_by_type_or_description() {
        for (t, d) in [
            (IF_TYPE_PPP, "RAS Async Adapter"),
            (IF_TYPE_TUNNEL, "Teredo Tunneling Pseudo-Interface"),
            (IF_TYPE_PROP_VIRTUAL, "Anything"),
            (6, "WireGuard Tunnel"),
            (6, "Wintun Userspace Tunnel"),
            (6, "TAP-Windows Adapter V9"),
            (6, "OpenVPN Data Channel Offload"),
            (6, "Cisco AnyConnect Secure Mobility Client Virtual Miniport Adapter"),
            (6, "PANGP Virtual Ethernet Adapter Secure"),
            (6, "Fortinet Virtual Ethernet Adapter (NDIS 6.30)"),
            (6, "NordLynx Tunnel"),
            (6, "Tailscale Tunnel"),
            (6, "ZeroTier Virtual Port"),
            (6, "Acme VPN Client"),
        ] {
            assert!(tunnel_adapter(t, d), "{t} {d}");
        }
        for (t, d) in [
            (6, "Intel(R) Ethernet Connection I219-V"),
            (71, "Intel(R) Wi-Fi 6 AX201 160MHz"),
        ] {
            assert!(!tunnel_adapter(t, d), "{t} {d}");
        }
    }

    #[test]
    fn pro_is_read_only_from_an_exact_flag() {
        let mut t = txt(HEX, "n", "linux", "1");
        assert!(!parse(&t, &[V4], 1).unwrap().pro, "absent means Free");
        t.push(("pro", "yes"));
        assert!(!parse(&t, &[V4], 1).unwrap().pro);
        t.pop();
        t.push(("pro", "1"));
        assert!(parse(&t, &[V4], 1).unwrap().pro);
    }
}
