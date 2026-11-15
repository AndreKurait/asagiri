// Each revision is compiled separately with the same crate and module names.
// The parent alternates requests to otherwise idle benchmark processes.
// Only allocation/release is timed; IPC and fixture setup are excluded.
use std::collections::{BTreeMap, BTreeSet};
use std::hint::black_box;
use std::io::{self, BufRead, Write};
use std::time::Instant;

struct Case {
    family: u8,
    pattern: &'static str,
    size: u32,
    loops: u32,
}

// MODULES

fn main() {
    let input = io::stdin();
    let mut output = io::stdout().lock();
    for line in input.lock().lines() {
        let line = line.unwrap();
        let mut parts = line.split_whitespace();
        let family = parts.next().unwrap().parse().unwrap();
        let pattern = match parts.next().unwrap() {
            "dense" => "dense", "sparse" => "sparse", "late-range" => "late-range",
            "avoid" => "avoid", "mixed" => "mixed", "aux" => "aux", "full" => "full",
            _ => panic!("Unknown benchmark fixture"),
        };
        let size = parts.next().unwrap().parse().unwrap();
        let loops = parts.next().unwrap().parse().unwrap();
        assert!(parts.next().is_none());
        assert!(matches!(family, 4 | 6) && size <= 65535 && (1..=2000).contains(&loops));
        let case = Case { family, pattern, size, loops };
        let (elapsed, address) = revision::implementation::batch(&case, case.loops);
        let value = match address { Some(value) => format!("{value:032x}"), None => "none".into() };
        writeln!(output, "{{\"ns\":{elapsed},\"address\":\"{value}\"}}").unwrap();
        output.flush().unwrap();
    }
}
