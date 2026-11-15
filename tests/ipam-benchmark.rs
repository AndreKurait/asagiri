// The runner adds two modules containing allocation methods copied verbatim
// from the compared revisions. This harness never starts an engine or VM.
use std::collections::{BTreeMap, BTreeSet};
use std::hint::black_box;
use std::time::Instant;

struct Case {
    family: u8,
    pattern: &'static str,
    size: u32,
    loops: u32,
}

// MODULES

fn main() {
    let pairs: usize = std::env::args().nth(1).unwrap().parse().unwrap();
    let mut cases = Vec::new();
    for family in [4, 6] {
        for size in [0, 8, 32, 256, 1024, 4096, 65532] {
            let loops = match size { 0..=32 => 2000, 33..=256 => 500, 257..=4096 => 50, _ => 4 };
            cases.push(Case { family, pattern: "dense", size, loops });
        }
        cases.push(Case { family, pattern: "sparse", size: 65532, loops: 1000 });
        cases.push(Case { family, pattern: "late-range", size: 32767, loops: 1000 });
        cases.push(Case { family, pattern: "avoid", size: 4096, loops: 8 });
        cases.push(Case { family, pattern: "mixed", size: 4096, loops: 16 });
        cases.push(Case { family, pattern: "aux", size: 256, loops: 16 });
        cases.push(Case { family, pattern: "full", size: 65535, loops: 4 });
    }
    for case in cases {
        let expected = baseline::implementation::batch(&case, 1).1;
        assert_eq!(candidate::implementation::batch(&case, 1).1, expected);
        // Exclude five complete paired warmup batches from the measurements.
        for index in 0..pairs + 5 {
            let (a, b) = if index % 2 == 0 {
                let a = baseline::implementation::batch(&case, case.loops);
                let b = candidate::implementation::batch(&case, case.loops);
                (a, b)
            } else {
                let b = candidate::implementation::batch(&case, case.loops);
                let a = baseline::implementation::batch(&case, case.loops);
                (a, b)
            };
            assert_eq!(a.1, expected);
            assert_eq!(b.1, expected);
            if index >= 5 {
                let value = match expected { Some(value) => format!("{value:032x}"), None => "none".into() };
                println!("{{\"case\":\"ipv{}-{}-{}\",\"pair\":{},\"baseline_first\":{},\"loops\":{},\"baseline_ns\":{},\"candidate_ns\":{},\"address\":\"{}\"}}",
                         case.family, case.pattern, case.size, index - 5, index % 2 == 0, case.loops, a.0, b.0, value);
            }
        }
    }
}
