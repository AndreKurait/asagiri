//! Unsigned decimal Content-Length and hexadecimal chunk-size parsing.
//! The Verus contract uses this executable body, including its overflow check.

pub(super) fn parse_number(bytes: &[u8], hex: bool) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let radix = if hex { 16u128 } else { 10u128 };
    let mut value = 0u64;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        let digit = match byte {
            b'0'..=b'9' => (byte - b'0') as u128,
            b'a'..=b'f' if hex => (byte - b'a' + 10) as u128,
            b'A'..=b'F' if hex => (byte - b'A' + 10) as u128,
            _ => {
                // Reject the first invalid digit without scanning the suffix.
                return None;
            },
        };
        let next = value as u128 * radix + digit;
        if next > u64::MAX as u128 {
            // A later digit cannot make an overflowing prefix representable.
            return None;
        }
        value = next as u64;
        index += 1;
    }
    Some(value)
}
