use vstd::prelude::*;

verus! {

fn is_current(current: u64, captured: u64) -> (result: bool)
    ensures result == (current as int == captured as int),
{
/* EXECUTABLE_BODY */
}

fn main() {
    let current = is_current(0, 0);
    assert(current);
    let stale = is_current(1, 0);
    assert(!stale);
    let wrapped = is_current(0, u64::MAX);
    assert(!wrapped);
}

}
