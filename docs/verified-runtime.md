# Verified runtime transitions

Asagiri uses Verus to check selected production functions in addition to its
[IPAM search](verified-ipam.md). These are function contracts with explicit
boundaries; they do not certify the entire engine or its Docker compatibility.

## Health results

`Health::record` calls `health_transition::after_probe` to update the consecutive
failure count and health status. For all Boolean inputs and every `u32` failure
count and retry threshold, Verus proves:

- Success resets the count to zero and makes the container healthy.
- Failure during the start period preserves both count and status while the
  container is starting.
- Every other failure increments the count, saturating at `u32::MAX`.
- A counted failure makes the container unhealthy exactly when the resulting
  count is at least the retry threshold; otherwise it preserves the status.
- Failure never decreases the count.

The production regression calls `Health::record` at the maximum count, crosses
the retry and start-period boundaries, and then recovers on success. Before
this change, the unchecked increment panicked in the same production method
with overflow checks enabled; optimized execution could wrap the count.

The proof does not cover probe scheduling, clock conversion, command execution,
container generations or OS process cancellation. Existing module tests and
the live health, lifecycle and Compose checks exercise those surrounding paths.
Native Mac VM and Windows WSL results must be recorded separately from Linux
runtime results.

## Layer transfer admission

`LayerTransfers::run` claims a digest and a capacity slot in one state lock.
Waiters own neither resource and can observe cancellation every 50 ms without
waiting for the transfer that holds the digest or capacity. Success, errors
and unwinding release both resources. The next transfer checks whether the
layer is already present before uploading it.

Verus checks the exact `layer_admission::transition` function used for both
claiming and releasing permits. For every `usize` count and limit, it proves:

- A claim succeeds exactly when the count is valid, capacity remains, the
  digest is available, and the sampled cancellation flag is false.
- A successful claim adds exactly one; release subtracts exactly one.
- Release succeeds for a valid nonzero count even when cancellation is set.
- Empty releases, over-capacity states and blocked claims return no transition.
- Every successful result stays within capacity, without arithmetic overflow.

Six broken transitions must fail verification, covering cancellation, digest
ownership, capacity, accounting, empty releases and invalid input states.
Concrete witnesses include zero capacity and maximum machine-sized counts.

The arithmetic proof does not verify Rust's mutexes, condvars, hash set or
destructors, or establish scheduler fairness. Production synchronization tests
exercise shared-digest contention, independent transfers, capacity limits,
error and panic cleanup, cancellation while waiting, and retry. Both waiting
cancellation regressions fail against the previous production locking code:
the request cannot finish until the unrelated owner is released.

Cancellation is cooperative. A flag that changes after the admission sample
is checked again by the engine before work starts; this does not interrupt an
already executing filesystem or transport operation. Layer durability, guest
retirement, stale process completion and native VM/WSL behavior retain their
separate runtime qualification requirements.

## HTTP framing numbers

`http_number::parse_number` parses the decimal `Content-Length` and hexadecimal
chunk sizes used by the production HTTP reader. For every byte slice and either
mode, Verus proves that it:

- Accepts nonempty sequences of decimal digits, or hexadecimal digits in hex
  mode, including leading zeroes and either letter case.
- Returns their exact mathematical value when it fits in `u64`.
- Rejects empty input, signs, spaces, other non-digits and overflowing values.
- Cannot overflow its intermediate arithmetic or index beyond the input.
- Terminates, advancing once per digit and stopping at the first invalid or
  overflowing prefix.

The contract defines a mathematical prefix value, and proves that a rejected
prefix cannot be repaired by a suffix. The executable parser uses the original
input directly, without allocating a parsed representation. Only ghost
invariants and proof blocks are inserted into the production body; removing
those insertions must recover its original bytes.

Six broken parser variants must fail proof verification: accepting empty input,
accepting an invalid digit, ignoring overflow, using decimal base for hex,
misreading uppercase letters and discarding a parsed digit. Production-module
tests compare every one- and two-byte input with the standard Rust parser plus
HTTP's digit restrictions. Verus also proves that prepending any number of
zero digits preserves the mathematical prefix value in both bases.

This function proof does not establish header tokenization, connection closure,
stream cancellation, memory bounds in the surrounding HTTP reader or the
Windows named-pipe bridge. Existing production HTTP tests cover ambiguous
framing, truncation, bounded chunk lines and trailers, streamed archives and
subsequent requests. Platform and live API results still have to qualify the
complete engine.

## Source binding and negative controls

`scripts/ci/verify-runtime.py` inserts the executable function body directly from
its production source file into a contract in `proofs/`. The expected signature
and file boundaries are checked before insertion. There is no separately
maintained executable implementation in the proof. Loop and ghost annotations
are hashed in the result alongside the original and generated source.

Verification uses the Rust, Verus and solver versions and archive SHA-256
already pinned for IPAM in `scripts/ci/verus.json`, with `--no-cheating`.
Concrete calls establish non-vacuous boundary cases. Six deliberately broken
health implementations must fail the prover: an overflowing counter, an
uncounted failure, a late retry threshold, a success that retains its count,
an overly broad grace period and a failure reported as healthy. Compilation
failure or solver resource exhaustion does not satisfy a negative control.
The runner requires the expected proof diagnostic and clears environment
overrides for Verus's libraries and solvers. Cleanup failures are recorded
without replacing an earlier proof failure.

Rust, Verus, the solver and Verus's standard-library specifications are trusted
components. The caller's Boolean inputs are not claims about real elapsed time
or whether the probe command actually succeeded.

## Reproduce

```sh
rustup toolchain install 1.99.1 --profile minimal --no-self-update
python3 scripts/ci/verify-runtime.py --cache /tmp/asagiri-verus --output /tmp/asagiri-runtime-proof
```

Use a new output directory for each run. The required Linux platform job runs
the proofs and all negative controls, then preserves the generated sources,
proof output, source and template hashes, tested revision and tool identities
in the `runtime-proof-X64` artifact with 90-day requested retention. A dirty
worktree is identified in the local result rather than attributed to its
unchanged HEAD. The runner fingerprints the production files, proof templates,
annotations, toolchain pin and runner scripts before reading inputs, after
reading them and after verification. A second edit to an already dirty file
therefore fails the run even when its Git status entry stays the same.
