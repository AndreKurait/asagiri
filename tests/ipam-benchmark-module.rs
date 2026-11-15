// This fixture is instantiated identically for both source revisions.
use super::super::{black_box, BTreeMap, BTreeSet, Case, Instant};

struct Network {
    subnet: [u8; 4],
    prefix: u8,
    gateway: [u8; 4],
    ip_range: Option<([u8; 4], u8)>,
    aux: BTreeMap<String, [u8; 4]>,
    used: BTreeSet<u32>,
}

struct V6 {
    subnet: [u8; 16],
    prefix: u8,
    gateway: [u8; 16],
    ip_range: Option<([u8; 16], u8)>,
    aux: BTreeMap<String, [u8; 16]>,
    used: BTreeSet<u128>,
}

// PRODUCTION_METHODS

fn occupied(case: &Case, last: u32) -> BTreeSet<u32> {
    match case.pattern {
        "dense" => (2..2 + case.size).collect(),
        "sparse" => (3..=last).collect(),
        "late-range" => (1..32768).collect(),
        "mixed" => (2..2 + case.size).step_by(2).collect(),
        "full" => (2..=last).collect(),
        "avoid" | "aux" => BTreeSet::new(),
        _ => unreachable!(),
    }
}

fn excluded(case: &Case) -> Vec<u32> {
    match case.pattern {
        "avoid" => (2..2 + case.size).rev().collect(),
        "mixed" => (3..2 + case.size).step_by(2).rev().collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn batch(case: &Case, loops: u32) -> (u128, Option<u128>) {
    if case.family == 4 {
        let base = u32::from_be_bytes([10, 5, 0, 0]);
        let mut network = Network {
            subnet: base.to_be_bytes(), prefix: 16, gateway: (base + 1).to_be_bytes(),
            ip_range: (case.pattern == "late-range").then_some(((base + 32768).to_be_bytes(), 17)),
            aux: if case.pattern == "aux" { (2..2 + case.size).map(|h| (format!("{h:08}"), (base + h).to_be_bytes())).collect() } else { BTreeMap::new() },
            used: occupied(case, 65534),
        };
        let avoid: Vec<_> = excluded(case).into_iter().map(|h| (base + h).to_be_bytes()).collect();
        let mut result = None;
        let start = Instant::now();
        for _ in 0..loops {
            result = black_box(&mut network).allocate_avoiding(black_box(&avoid));
            if let Some(ip) = result { network.release(black_box(ip)); }
            black_box(result);
        }
        let elapsed = start.elapsed().as_nanos();
        (elapsed, result.map(|ip| u32::from_be_bytes(ip) as u128))
    } else {
        // The subnet reaches u128::MAX and includes its last address.
        let base = u128::MAX - 65535;
        let mut network = V6 {
            subnet: base.to_be_bytes(), prefix: 112, gateway: (base + 1).to_be_bytes(),
            ip_range: (case.pattern == "late-range").then_some(((base + 32768).to_be_bytes(), 113)),
            aux: if case.pattern == "aux" { (2..2 + case.size).map(|h| (format!("{h:08}"), (base + h as u128).to_be_bytes())).collect() } else { BTreeMap::new() },
            used: occupied(case, 65535).into_iter().map(u128::from).collect(),
        };
        let avoid: Vec<_> = excluded(case).into_iter().map(|h| (base + h as u128).to_be_bytes()).collect();
        let mut result = None;
        let start = Instant::now();
        for _ in 0..loops {
            result = black_box(&mut network).allocate_avoiding(black_box(&avoid));
            if let Some(ip) = result { network.release(black_box(ip)); }
            black_box(result);
        }
        let elapsed = start.elapsed().as_nanos();
        (elapsed, result.map(u128::from_be_bytes))
    }
}
