use vstd::prelude::*;

verus! {

fn transition(active: usize, limit: usize, acquiring: bool, busy: bool, cancelled: bool)
    -> (result: Option<usize>)
    ensures
        result.is_some() == (
            active <= limit && if acquiring {
                !cancelled && !busy && active < limit
            } else {
                active > 0
            }
        ),
        match result {
            Some(next) => next <= limit
                && next as int == if acquiring { active as int + 1 } else { active as int - 1 },
            None => true,
        },
{
/* EXECUTABLE_BODY */
}

fn main() {
    let first = transition(0, 1, true, false, false);
    assert(first == Some(1usize));
    let full = transition(1, 1, true, false, false);
    assert(full == None::<usize>);
    let busy = transition(0, 1, true, true, false);
    assert(busy == None::<usize>);
    let cancelled = transition(0, 1, true, false, true);
    assert(cancelled == None::<usize>);
    let released = transition(1, 1, false, true, true);
    assert(released == Some(0usize));
    let empty = transition(0, 1, false, false, false);
    assert(empty == None::<usize>);
    let no_capacity = transition(0, 0, true, false, false);
    assert(no_capacity == None::<usize>);
    let last = transition(usize::MAX - 1, usize::MAX, true, false, false);
    assert(last == Some(usize::MAX));
    let overflow = transition(usize::MAX, usize::MAX, true, false, false);
    assert(overflow == None::<usize>);
    let invalid = transition(2, 1, false, false, false);
    assert(invalid == None::<usize>);
}

}
