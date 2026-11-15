// The driver inserts the production function's body, adding only the
// invariant from address-gap-loop.txt. There is no executable copy here.
use vstd::prelude::*;
use vstd::std_specs::iter::IteratorSpec;

verus! {

pub fn first_gap<I: Iterator<Item = u128>>(occupied: I, first: u128, last: u128) -> (result: Option<u128>)
    requires
        occupied.obeys_prophetic_iter_laws(),
        occupied.decrease() is Some,
        forall|i: int, j: int| 0 <= i <= j < occupied.remaining().len()
            ==> occupied.remaining()[i] <= occupied.remaining()[j],
    ensures
        match result {
            Some(value) => {
                &&& first <= value <= last
                &&& !occupied.remaining().contains(value)
                &&& forall|a: u128| first <= a < value ==> occupied.remaining().contains(a)
                &&& value - first <= occupied.remaining().len()
            },
            None => {
                &&& forall|a: u128| first <= a <= last ==> occupied.remaining().contains(a)
                &&& first <= last ==> last - first + 1 <= occupied.remaining().len()
            },
        },
{
/* EXECUTABLE_BODY */
}

// These checked calls establish that the preconditions admit ordinary,
// empty, duplicate, exhausted, and maximum-address inputs.
fn witnesses() {
    let ordinary = vec![0u128, 1, 1, 2, 4];
    let ghost values = ordinary@;
    let result = first_gap(ordinary.into_iter(), 1, 9);
    assert(!values.contains(3u128));
    match result {
        Some(value) => {
            if value < 3 { assert(values.contains(value)); }
            if value > 3 { assert(values.contains(3u128)); }
        },
        None => { assert(values.contains(3u128)); },
    }
    assert(result == Some(3u128));
    let empty: Vec<u128> = Vec::new();
    let result = first_gap(empty.into_iter(), u128::MAX, u128::MAX);
    assert(result == Some(u128::MAX));
    let full = vec![u128::MAX];
    let result = first_gap(full.into_iter(), u128::MAX, u128::MAX);
    assert(result == None);
    let reversed: Vec<u128> = Vec::new();
    let result = first_gap(reversed.into_iter(), 5, 4);
    assert(result == None);
}

}
fn main() {}
