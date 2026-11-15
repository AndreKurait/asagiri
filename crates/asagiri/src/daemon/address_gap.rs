//! Find the lowest unused address without a lookup for every occupied host.
//!
//! `occupied` must be finite and sorted in ascending order; duplicates are
//! allowed. Callers merge ordered ranges of occupied and excluded hosts. Values outside
//! the requested range are harmless. The search visits each entry at most
//! once and never enumerates a gap or overflows at the last IPv6 address.
//!
//! scripts/ci/verify-ipam.py verifies this exact function body with Verus.

pub(super) fn first_gap<I: Iterator<Item = u128>>(occupied: I, first: u128, last: u128) -> Option<u128> {
    if first > last {
        return None;
    }
    let mut candidate = first;
    for address in occupied {
        if address > candidate {
            return Some(candidate);
        }
        if address == candidate {
            if candidate == last {
                return None;
            }
            candidate += 1;
        }
    }
    Some(candidate)
}
