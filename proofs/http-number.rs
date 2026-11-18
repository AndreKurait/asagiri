use vstd::prelude::*;

verus! {

spec fn digit(byte: u8, hex: bool) -> int {
    if b'0' <= byte <= b'9' {
        byte as int - b'0' as int
    } else if hex && b'a' <= byte <= b'f' {
        byte as int - b'a' as int + 10
    } else if hex && b'A' <= byte <= b'F' {
        byte as int - b'A' as int + 10
    } else {
        -1
    }
}

// A mathematical base-10/base-16 prefix; None means a non-digit or a value
// outside the unsigned 64-bit range. Leading zeroes remain valid.
spec fn prefix(bytes: Seq<u8>, hex: bool, end: int) -> Option<int>
    recommends 0 <= end <= bytes.len(),
    decreases end,
{
    if end <= 0 {
        Some(0)
    } else {
        match prefix(bytes, hex, end - 1) {
            None => None,
            Some(previous) => {
                let d = digit(bytes[end - 1], hex);
                let next = previous * (if hex { 16int } else { 10int }) + d;
                if d < 0 || next > u64::MAX {
                    None
                } else {
                    Some(next)
                }
            },
        }
    }
}

proof fn rejected_prefix_stays_rejected(bytes: Seq<u8>, hex: bool, at: int, end: int)
    requires
        0 <= at <= end <= bytes.len(),
        prefix(bytes, hex, at) == None,
    ensures prefix(bytes, hex, end) == None,
    decreases end - at,
{
    if at < end {
        rejected_prefix_stays_rejected(bytes, hex, at, end - 1);
    }
}

fn parse_number(bytes: &[u8], hex: bool) -> (result: Option<u64>)
    ensures
        match result {
            Some(value) => bytes@.len() > 0
                && prefix(bytes@, hex, bytes@.len() as int) == Some(value as int),
            None => bytes@.len() == 0 || prefix(bytes@, hex, bytes@.len() as int) == None,
        },
{
/* EXECUTABLE_BODY */
}

fn main() {
    reveal_with_fuel(prefix, 4);
    let empty: [u8; 0] = [];
    let parsed = parse_number(&empty, false);
    assert(parsed == None);
    let zero = [b'0'];
    let parsed = parse_number(&zero, false);
    assert(parsed == Some(0u64));
    let mixed = [b'f', b'F'];
    let parsed = parse_number(&mixed, true);
    assert(parsed == Some(255u64));
    let parsed = parse_number(&mixed, false);
    assert(parsed == None);
    let sign = [b'+', b'1'];
    let parsed = parse_number(&sign, false);
    assert(parsed == None);
}

}
