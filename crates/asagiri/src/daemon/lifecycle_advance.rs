//! Invalidate the current generation, including at the counter boundary.

pub(super) fn advance(current: u64) -> u64 {
    if current == u64::MAX { 0 } else { current + 1 }
}
