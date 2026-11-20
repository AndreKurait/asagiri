use vstd::prelude::*;

verus! {

fn advance(current: u64) -> (next: u64)
    ensures
        next != current,
        next as int == if current == u64::MAX { 0 } else { current as int + 1 },
{
/* EXECUTABLE_BODY */
}

fn main() {
    let next = advance(u64::MAX);
    assert(next == 0);
}

}
