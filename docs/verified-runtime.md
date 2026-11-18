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
  count reaches the retry threshold; otherwise it preserves the status.
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

## Source binding and negative controls

`scripts/ci/verify-runtime.py` inserts the executable function body directly from
its production source file into a contract in `proofs/`. The expected signature
and file boundaries are checked before insertion. There is no separately
maintained executable implementation in the proof.

Verification uses the Rust, Verus and solver versions and archive SHA-256
already pinned for IPAM in `scripts/ci/verus.json`, with `--no-cheating`.
Concrete calls establish non-vacuous boundary cases. Six deliberately broken
health implementations must fail the prover: an overflowing counter, an
uncounted failure, a late retry threshold, a success that retains its count,
an overly broad grace period and a failure reported as healthy. Compilation
failure alone does not satisfy a negative control.

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
unchanged HEAD.
