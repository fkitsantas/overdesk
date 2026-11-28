// SPDX-License-Identifier: AGPL-3.0-only
//! The devices heard on the network: folding discovery changes in, listing nearby and paired
//! devices, and picking which one to dial.

use std::collections::{BTreeMap, BTreeSet};

use overdesk_net::discovery::{Change, Nearby};
use overdesk_net::pairing::{Fingerprint, peer_id};
use overdesk_proto::PeerId;
use tokio::time::Instant;

use super::model::{DeviceKind, NearbyDevice, PairedDevice};
use super::persist::clean_name;

/// How many devices that are not paired are remembered from the network at once.
const MAX_UNPAIRED_NEARBY: usize = 64;

/// Picks the paired nearby computer to dial automatically: only those with a greater id
/// than `me` (the lower id dials, so two devices never dial each other at once), not
/// in `left` and not backing off. The lowest id wins, so the choice is deterministic.
pub fn auto_target(
    me: PeerId,
    nearby: &[NearbyDevice],
    retry_after: &BTreeMap<PeerId, Instant>,
    left: &BTreeSet<PeerId>,
    now: Instant,
) -> Option<NearbyDevice> {
    nearby
        .iter()
        .filter(|d| d.kind == DeviceKind::Computer && d.paired && d.id > me && !left.contains(&d.id))
        .filter(|d| retry_after.get(&d.id).is_none_or(|t| now >= *t))
        .min_by_key(|d| d.id)
        .cloned()
}

/// Folds one network change into the devices heard so far: a device is kept by id, so
/// re-announcing replaces it, and `Lost` removes it. The address is only a hint, since
/// anyone can announce any id; it is verified against the pin when connecting.
pub fn apply(found: &mut BTreeMap<PeerId, Nearby>, change: Change) {
    match change {
        Change::Found(n) => {
            found.insert(n.id, n);
        }
        Change::Lost(id) => {
            found.remove(&id);
        }
    }
}

/// Like [`apply`], but a device that is not paired is ignored once [`MAX_UNPAIRED_NEARBY`]
/// others are remembered, so announcements from a hostile network cannot grow the list without
/// bound. Paired devices are always kept.
pub(super) fn apply_capped(found: &mut BTreeMap<PeerId, Nearby>, change: Change, pins: &BTreeSet<Fingerprint>) {
    if let Change::Found(n) = &change
        && !found.contains_key(&n.id)
    {
        let paired: BTreeSet<PeerId> = pins.iter().map(peer_id).collect();
        let unpaired = found.keys().filter(|id| !paired.contains(id)).count();
        if unpaired >= MAX_UNPAIRED_NEARBY && !paired.contains(&n.id) {
            return;
        }
    }
    apply(found, change);
}

/// The list the UI shows: sorted by name, each device marked paired when a pinned
/// fingerprint starts with its id (a [`PeerId`] is a fingerprint's first 16 bytes).
pub fn nearby_list(found: &BTreeMap<PeerId, Nearby>, pins: &BTreeSet<Fingerprint>) -> Vec<NearbyDevice> {
    let paired: BTreeSet<PeerId> = pins.iter().map(peer_id).collect();
    let mut list: Vec<_> = found
        .values()
        .map(|n| NearbyDevice {
            kind: DeviceKind::Computer,
            name: n.name.clone(),
            os: n.os,
            address: n.address,
            id: n.id,
            paired: paired.contains(&n.id),
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

/// Remembers the names of the paired devices in `nearby` only: an unpaired one is any host on the
/// network, so keeping its name would let announcements grow the cache without bound.
/// A name already known is kept: only the peer itself, over its authenticated link, may rename
/// it ([`Shared::learn_name`]). Returns whether `names` changed.
pub(super) fn remember_names(names: &mut BTreeMap<PeerId, String>, nearby: &[NearbyDevice]) -> bool {
    let before = names.len();
    for d in nearby.iter().filter(|d| d.paired) {
        let name = clean_name(&d.name);
        if !name.is_empty() {
            names.entry(d.id).or_insert(name);
        }
    }
    names.len() != before
}

/// The paired computers, sorted by name: each is named from `names`, else by
/// the start of its id, and is online when `nearby` lists it.
pub fn paired_list(
    pins: &BTreeSet<Fingerprint>,
    nearby: &[NearbyDevice],
    names: &BTreeMap<PeerId, String>,
) -> Vec<PairedDevice> {
    let ids: BTreeSet<PeerId> = pins.iter().map(peer_id).collect();
    let mut list: Vec<_> = ids
        .into_iter()
        .map(|id| PairedDevice {
            id,
            name: names.get(&id).cloned().unwrap_or_else(|| id.to_hex()[..8].to_owned()),
            kind: DeviceKind::Computer,
            online: nearby.iter().any(|d| d.id == id),
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use overdesk_proto::Os;
    use std::net::SocketAddr;
    use std::time::Duration;

    #[test]
    fn names_are_kept_for_paired_devices_only() {
        let found: BTreeMap<_, _> = (0..1000u16)
            .map(|n| {
                let mut id = [0u8; 16];
                id[..2].copy_from_slice(&n.to_be_bytes());
                (
                    PeerId(id),
                    Nearby {
                        id: PeerId(id),
                        name: format!("d{n}"),
                        os: Os::Linux,
                        address: "127.0.0.1:1".parse().unwrap(),
                        pro: false,
                    },
                )
            })
            .collect();
        let mut names = BTreeMap::new();
        remember_names(&mut names, &nearby_list(&found, &BTreeSet::new()));
        assert!(names.is_empty());
        let one = *found.keys().next().unwrap();
        let mut fp = [0u8; 32];
        fp[..16].copy_from_slice(&one.0);
        remember_names(&mut names, &nearby_list(&found, &BTreeSet::from([fp])));
        assert_eq!(names.len(), 1);
    }

    /// A device seen on the network, with a one-byte id and an address derived from it.
    fn seen(n: u8, name: &str) -> Nearby {
        Nearby {
            id: PeerId([n; 16]),
            name: name.into(),
            os: Os::Windows,
            address: SocketAddr::from(([10, 0, 0, n], 24801)),
            pro: false,
        }
    }

    /// A nearby device with a one-byte id.
    fn dev(n: u8, paired: bool) -> NearbyDevice {
        NearbyDevice {
            kind: DeviceKind::Computer,
            name: format!("D{n}"),
            os: Os::Windows,
            address: SocketAddr::from(([10, 0, 0, n], 24801)),
            id: PeerId([n; 16]),
            paired,
        }
    }

    #[test]
    fn a_network_name_only_fills_a_gap() {
        let mut names = BTreeMap::from([(PeerId([1; 16]), "Authentic".to_owned())]);
        let heard = [dev(1, true), dev(2, true), dev(3, false)];
        assert!(remember_names(&mut names, &heard), "the missing paired one was added");
        assert_eq!(names[&PeerId([1; 16])], "Authentic");
        assert_eq!(names[&PeerId([2; 16])], "D2");
        assert!(!names.contains_key(&PeerId([3; 16])));
        assert!(!remember_names(&mut names, &heard), "nothing new, nothing to save");
    }

    #[test]
    fn unpaired_devices_are_capped_but_paired_ones_always_fit() {
        let mut found = BTreeMap::new();
        let mut fp = [0u8; 32];
        fp[..16].fill(200);
        let pins = BTreeSet::from([fp]);
        let id = |n: u8| PeerId([n; 16]);
        for n in 0..(MAX_UNPAIRED_NEARBY as u8 + 10) {
            apply_capped(&mut found, Change::Found(seen(n, "x")), &pins);
        }
        assert_eq!(found.len(), MAX_UNPAIRED_NEARBY);
        apply_capped(&mut found, Change::Found(seen(200, "mine")), &pins);
        assert!(found.contains_key(&id(200)), "a paired device is kept");
        // Re-announcing one already known is not a new entry.
        apply_capped(&mut found, Change::Found(seen(3, "renamed")), &pins);
        assert_eq!(found[&id(3)].name, "renamed");
        apply_capped(&mut found, Change::Lost(id(3)), &pins);
        assert!(!found.contains_key(&id(3)));
    }

    #[test]
    fn only_the_lower_id_dials_a_paired_device() {
        let (none, left, now) = (BTreeMap::new(), BTreeSet::new(), Instant::now());
        let nearby = [dev(1, true), dev(5, false), dev(7, true), dev(9, true)];
        // Lowest greater paired id; the unpaired 5 and the lower 1 are skipped.
        let t = auto_target(PeerId([3; 16]), &nearby, &none, &left, now);
        assert_eq!(t.map(|d| d.id), Some(PeerId([7; 16])));
        // The higher side never dials.
        assert!(auto_target(PeerId([9; 16]), &nearby, &none, &left, now).is_none());
    }

    #[test]
    fn a_remote_device_is_never_dialed_automatically() {
        let mut remote = dev(7, true);
        remote.kind = DeviceKind::Remote { locked: false };
        let t = auto_target(
            PeerId([3; 16]),
            &[remote],
            &BTreeMap::new(),
            &BTreeSet::new(),
            Instant::now(),
        );
        assert!(t.is_none());
    }

    #[test]
    fn backoff_and_left_peers_are_skipped() {
        let now = Instant::now();
        let nearby = [dev(7, true), dev(9, true)];
        let me = PeerId([3; 16]);
        let retry = BTreeMap::from([(PeerId([7; 16]), now + Duration::from_secs(5))]);
        let t = auto_target(me, &nearby, &retry, &BTreeSet::new(), now);
        assert_eq!(t.map(|d| d.id), Some(PeerId([9; 16])));
        // Once the backoff has passed it is eligible again.
        let t = auto_target(me, &nearby, &retry, &BTreeSet::new(), now + Duration::from_secs(5));
        assert_eq!(t.map(|d| d.id), Some(PeerId([7; 16])));
        let left = BTreeSet::from([PeerId([7; 16]), PeerId([9; 16])]);
        assert!(auto_target(me, &nearby, &BTreeMap::new(), &left, now).is_none());
    }

    #[test]
    fn list_is_sorted_by_name_and_deduplicated_by_id() {
        let mut found = BTreeMap::new();
        for n in [seen(1, "Zed"), seen(2, "Alpha"), seen(1, "Zed renamed")] {
            apply(&mut found, Change::Found(n));
        }
        let names: Vec<_> = nearby_list(&found, &BTreeSet::new())
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, ["Alpha", "Zed renamed"]);
    }

    #[test]
    fn lost_removes_the_entry() {
        let mut found = BTreeMap::new();
        apply(&mut found, Change::Found(seen(1, "A")));
        apply(&mut found, Change::Lost(PeerId([1; 16])));
        apply(&mut found, Change::Lost(PeerId([9; 16])));
        assert!(nearby_list(&found, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn paired_means_a_pin_starts_with_the_peer_id() {
        let mut found = BTreeMap::new();
        apply(&mut found, Change::Found(seen(1, "Known")));
        apply(&mut found, Change::Found(seen(2, "Stranger")));
        let mut fp = [0u8; 32];
        fp[..16].fill(1);
        fp[16..].fill(0xff); // the rest of the fingerprint must not matter
        let list = nearby_list(&found, &BTreeSet::from([fp]));
        assert_eq!(
            list.iter().map(|d| (d.name.as_str(), d.paired)).collect::<Vec<_>>(),
            [("Known", true), ("Stranger", false)]
        );
        assert_eq!(list[0].address, SocketAddr::from(([10, 0, 0, 1], 24801)));
    }
}
