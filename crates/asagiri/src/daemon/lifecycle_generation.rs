//! Compare a captured generation while holding the same container lifecycle lock.

pub(super) fn is_current(current: u64, captured: u64) -> bool {
    current == captured
}
