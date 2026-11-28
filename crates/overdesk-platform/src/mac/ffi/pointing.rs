// SPDX-License-Identifier: AGPL-3.0-only
//! Which Apple pointing devices this Mac has, read from the IOKit registry.

use std::ffi::{c_char, c_void};
use std::ptr::NonNull;

use objc2_core_foundation::{CFNumber, CFRetained, CFString, CFType};
use overdesk_proto::{FEATURE_MAGIC_MOUSE, FEATURE_TRACKPAD};

use crate::mac::events::pointing_kind;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// A matching dictionary for every registry entry of class `name`; consumed by `IOServiceGetMatchingServices`.
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    /// Fills `existing` with an iterator over the entries `matching` matches (and consumes `matching`); 0 on success.
    fn IOServiceGetMatchingServices(main_port: u32, matching: *mut c_void, existing: *mut u32) -> i32;
    /// The next entry of `iterator`, or 0 at its end; the caller releases it.
    fn IOIteratorNext(iterator: u32) -> u32;
    /// A retained copy of property `key` of `entry` (null if it has none).
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: *const CFString,
        allocator: *const c_void,
        options: u32,
    ) -> *mut CFType;
    /// Releases an entry or iterator.
    fn IOObjectRelease(object: u32) -> i32;
}

/// Property `key` of `entry`, if it has one.
fn property(entry: u32, key: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(key);
    // SAFETY: `entry` is a live registry entry and `key` a valid CFString for the call; a
    // non-null result is a +1 reference this function now owns.
    let raw = unsafe { IORegistryEntryCreateCFProperty(entry, &raw const *key, std::ptr::null(), 0) };
    // SAFETY: `raw` is an owned CFType reference, as the "Create" rule promises.
    NonNull::new(raw).map(|p| unsafe { CFRetained::from_raw(p) })
}

/// The `FEATURE_MAGIC_MOUSE` and `FEATURE_TRACKPAD` bits for the pointing devices attached now.
pub fn pointing_features() -> u64 {
    let mut bits = 0;
    // SAFETY: the class name is a NUL-terminated literal; the dictionary it returns is consumed by
    // the next call, which fills `iter`.
    let iter = unsafe {
        let matching = IOServiceMatching(c"IOHIDDevice".as_ptr());
        let mut iter = 0;
        if matching.is_null() || IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return 0;
        }
        iter
    };
    loop {
        // SAFETY: `iter` is the live iterator above.
        let entry = unsafe { IOIteratorNext(iter) };
        if entry == 0 {
            break;
        }
        let number = |key| {
            property(entry, key)
                .and_then(|v| v.downcast::<CFNumber>().ok())
                .and_then(|n| n.as_i64())
                .unwrap_or(0)
        };
        let name = property(entry, "Product")
            .and_then(|v| v.downcast::<CFString>().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let (mouse, pad) = pointing_kind(number("VendorID"), number("ProductID"), &name);
        bits |= if mouse { FEATURE_MAGIC_MOUSE } else { 0 } | if pad { FEATURE_TRACKPAD } else { 0 };
        // SAFETY: `entry` came from `IOIteratorNext` and is released once.
        unsafe { IOObjectRelease(entry) };
    }
    // SAFETY: `iter` is released once, after its last use.
    unsafe { IOObjectRelease(iter) };
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_the_registry_does_not_crash_and_only_sets_known_bits() {
        assert_eq!(pointing_features() & !(FEATURE_MAGIC_MOUSE | FEATURE_TRACKPAD), 0);
    }
}
