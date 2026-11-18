use vstd::prelude::*;

verus! {

fn after_probe(success: bool, starting: bool, in_start_period: bool, failures: u32, retries: u32)
    -> (result: (u32, Option<bool>))
    ensures
        success ==> result == (0u32, Some(true)),
        !success && starting && in_start_period ==> result == (failures, None::<bool>),
        !success && !(starting && in_start_period) ==> (
            result.0 as int == if failures == u32::MAX { failures as int } else { failures as int + 1 }
            && result.1 == if result.0 >= retries { Some(false) } else { None::<bool> }
        ),
        !success ==> result.0 >= failures,
{
/* EXECUTABLE_BODY */
}

fn main() {
    let saturated = after_probe(false, false, false, u32::MAX, u32::MAX);
    assert(saturated == (u32::MAX, Some(false)));
    let reset = after_probe(true, true, true, u32::MAX, u32::MAX);
    assert(reset == (0u32, Some(true)));
    let grace = after_probe(false, true, true, 7, 1);
    assert(grace == (7u32, None::<bool>));
    let threshold = after_probe(false, false, true, 2, 3);
    assert(threshold == (3u32, Some(false)));
    let waiting = after_probe(false, true, false, 0, 3);
    assert(waiting == (1u32, None::<bool>));
}

}
