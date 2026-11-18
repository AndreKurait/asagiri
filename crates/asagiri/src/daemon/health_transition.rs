//! The health result transition, shared by the engine and its Verus contract.
//! `None` preserves the status; `Some` sets whether the container is healthy.

pub(super) fn after_probe(success: bool, starting: bool, in_start_period: bool, failures: u32, retries: u32) -> (u32, Option<bool>) {
    if success {
        return (0, Some(true));
    }
    if starting && in_start_period {
        return (failures, None);
    }
    let counted = if failures == u32::MAX { failures } else { failures + 1 };
    (counted, if counted >= retries { Some(false) } else { None })
}
