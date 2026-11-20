//! The health result transition, shared by the engine and its Verus contract.
//! `None` preserves the status; `Some` sets whether the container is healthy.

pub(super) fn after_probe(exit_code: i64, starting: bool, elapsed_ns: u128, start_period_ns: u128, failures: u32, retries: u32) -> (u32, Option<bool>) {
    if exit_code == 0 {
        return (0, Some(true));
    }
    if starting && elapsed_ns < start_period_ns {
        return (failures, None);
    }
    let counted = if failures == u32::MAX { failures } else { failures + 1 };
    (counted, if counted >= retries { Some(false) } else { None })
}
