# Verified address allocation

IPv4 and IPv6 allocation preserve Docker's lowest-free-address behavior,
including gateways, auxiliary reservations, static addresses, `--ip-range`,
exhaustion and reuse. Small and sparse networks use a short allocation-free
search. Dense networks merge ordered occupied and excluded addresses, avoiding
a tree lookup for every occupied host. The occupied tree is neither copied
nor scanned below the requested range.

## What Verus checks

`scripts/ci/verify-ipam.py` inserts the executable body from
`crates/asagiri/src/daemon/address_gap.rs` into the proof contract. It adds only
ghost loop invariants and checks that removing that insertion recovers the
original body bytes. A changed signature or insertion point fails the check.
The proof has no separately maintained implementation.

For every finite, ascending input sequence, including duplicates and values
outside the requested range, the search:

- Returns the lowest unused address between the inclusive bounds.
- Returns `None` exactly when that range has no unused address.
- Cannot overflow at `u128::MAX`.
- Advances its candidate no more than once per input entry and terminates
  under the iterator's finite, decreasing contract.

Verus also checks concrete calls with duplicate, empty, reversed, exhausted
and maximum-address inputs. Six intentionally broken production bodies must
fail proof verification, rather than merely fail to compile.

The verifier, bundled solver and standard-library specifications come from a
version and SHA-256 pinned in `scripts/ci/verus.json`. Verification uses
`--no-cheating`; no `assume`, `admit`, external-body escape or weakened check
is permitted in this proof. Rust, Verus, its standard-library specifications
and the solver remain trusted components.

The proof covers the gap-search function and its iterator contract.
Network parsing, the merge adapter, reservation collection, kernel networking,
concurrent API requests and Docker's behavior are outside that proof. Rust
regressions compare the actual adapter with an exhaustive set-membership
oracle and exercise both real allocation methods, including dense networks
whose IPv6 subnet ends at `u128::MAX`. Live Docker API tests cover network
integration, including Docker's HTTP 500 response for exhausted automatic
pools and a successful start/connect retry after an address is released.
This is a specific correctness proof, not a blanket Docker
compatibility or wall-clock performance guarantee.

## Reproduce

```sh
rustup toolchain install 1.99.1 --profile minimal --no-self-update
python3 scripts/ci/verify-ipam.py --cache /tmp/asagiri-verus --output /tmp/asagiri-ipam-proof
python3 scripts/ci/ipam-performance.py --baseline HEAD^1 --output /tmp/asagiri-ipam-performance
```

Use a new output directory for each performance run. The Linux platform job
runs both commands for each tested push or PR revision, and its success is
required by release planning. It preserves the proof results, generated source,
six failed negative controls, compiler/binary/source identities, and every
paired timing sample in a GHA artifact with requested retention of 90 days.

The benchmark extracts each revision's allocation, reservation and release
methods verbatim into identical standalone fixtures. Each revision uses the
same crate, module and input-file names in a separate executable; compiling
both into differently named modules can change optimization even when their
source is identical. Two otherwise idle processes answer alternating requests.
It measures actual
allocation and release on 26 IPv4/IPv6 cases: small and dense networks,
sparse occupancy, late ranges, exclusions, mixed exclusions and exhaustion.
Each case has five warmup batches and 100 pairs with alternating revision
order. Fixture construction and process communication are outside the timer.
CI also records an unchanged-source comparison and requires identical
executable hashes for identical generated source. Keep that control alongside
the comparison when interpreting timing differences; scheduling and hardware
noise can still affect either run. These pure Rust
microbenchmarks start no engine, app or VM.

IPAM timings are informational diagnostics, distinct from the existing
15-metric engine performance history and its regression gates. A faster
allocation microbenchmark does not establish an equivalent improvement in
container startup, application throughput or native VM performance.
