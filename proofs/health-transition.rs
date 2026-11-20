use vstd::prelude::*;

verus! {

fn after_probe(exit_code: i64, starting: bool, elapsed_ns: u128, start_period_ns: u128, failures: u32, retries: u32)
    -> (result: (u32, Option<bool>))
    ensures
        exit_code == 0 ==> result == (0u32, Some(true)),
        exit_code != 0 && starting && elapsed_ns < start_period_ns ==> result == (failures, None::<bool>),
        exit_code != 0 && !(starting && elapsed_ns < start_period_ns) ==> (
            result.0 as int == if failures == u32::MAX { failures as int } else { failures as int + 1 }
            && result.1 == if result.0 >= retries { Some(false) } else { None::<bool> }
        ),
        exit_code != 0 ==> result.0 >= failures,
{
/* EXECUTABLE_BODY */
}

fn main() {
    let boundary = after_probe(-1, true, 10, 10, u32::MAX, u32::MAX);
    assert(boundary == (u32::MAX, Some(false)));
}

}
