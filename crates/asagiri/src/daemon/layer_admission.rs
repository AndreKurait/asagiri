//! Count one layer permit without admitting cancelled or conflicting work.

pub(super) fn transition(active: usize, limit: usize, acquiring: bool, busy: bool, cancelled: bool) -> Option<usize> {
    if active > limit {
        return None;
    }
    if acquiring {
        if cancelled || busy || active == limit {
            return None;
        }
        Some(active + 1)
    } else if active == 0 {
        None
    } else {
        Some(active - 1)
    }
}
