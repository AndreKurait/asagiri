# Performance by commit

The [performance dashboard](https://andrekurait.github.io/asagiri/performance/)
tracks completed main builds. The `Speed` GitHub Actions workflow compares each
tested commit with its baseline on one GitHub-hosted Linux runner. Pull requests
compare with the merge commit's first parent; main pushes compare with the main
commit before the push. A push containing several commits measures its final
commit, not every intermediate commit.

## Measurements and regression gates

Both engines are freshly compiled with the same Rust toolchain and build flags.
Only dependencies and verified public container downloads are cached. Each
engine runs in its own disposable privileged container with fresh data and no
published ports. Ten container lifecycles warm each engine before measurement.

The workload alternates the order of the two engines across 400 iterations.
Each iteration measures an API ping, a container list, and a create/start/wait/
remove lifecycle running `true` in Alpine. The lifecycle uses the Docker API;
it does not include starting the Docker CLI, downloading an image, or booting a VM.

| Metric | Stored values | Regression requires both |
| --- | --- | --- |
| Container lifecycle | Median and p95, milliseconds | Median +10% and +0.5 ms |
| API ping | Median and p95, milliseconds | Median +15% and +0.05 ms |
| Container list | Median and p95, milliseconds | Median +15% and +0.1 ms |
| Idle daemon RSS | KiB | +10% and +2 MiB |
| Daemon RSS after iterations | KiB | +10% and +4 MiB |
| Daemon system calls per container | `strace` total over 20 additional lifecycles | +10% and +50 calls |
| Engine and agent binary sizes | Bytes and SHA-256 | Informational |

The baseline's checked-in harness supplies the limits. PRs cannot loosen their
own comparison thresholds. p95 and binary size are informative, without a
failure threshold. Resident memory and system-call counts can vary with
scheduling and asynchronous cleanup; they are not exact constants.
The syscall trace attaches to daemon threads and follows their new descendants.
It does not attach to already-running agents or measure all container syscalls.

`linux-engine-v2` pins the Alpine image by digest, stores all 2,400 individual
timing samples, uses nearest-rank p95, and rejects missing RSS measurements.
During rollout, a baseline with the previous harness produces a clearly labeled
`linux-engine-v1` record. That record has summaries only and used `alpine:3.21`;
it never claims raw samples or a pinned image. Workload and runner changes use
separate dashboard series.

## Durable history

Every completed comparison uploads `record.json`, `results.json`, `sizes.json`
and `report.md` as an artifact with a requested retention of 90 days. Each record
includes the tested commit, baseline, harness commit, binary hashes, run and
attempt IDs, measurements, regression limits and runner details. Main and manual
runs measure even when their freshly built binaries are identical; PRs can skip
that redundant measurement.

A separate `Performance history` workflow accepts completed official main
`Speed` runs, including measured regressions. It checks the source ancestry,
run and attempt, benchmark step outcomes, GitHub artifact digest, bounded JSON
schema, individual sample summaries and evidence checksums. It treats the
artifact as data and never executes downloaded files. Fork and PR results
cannot write the archive.

The publisher writes JSON to the dedicated `performance-history` branch using
the temporary workflow token. GitHub signs each data commit with GPG, and the
message includes a sign-off. It never commits measurements to main. This data
branch is intentionally retained; it is not a PR branch.

The dashboard reads the latest 500 records from the public index. Every
published raw record remains in `benchmarks/v1/runs/` on the data branch, beyond
GHA artifact expiration. Repeated publication is idempotent; it refuses to
overwrite a run with different data. Failed builds or interrupted measurements
do not become invented performance results.

## Read and reproduce results

Use the runner/workload selector to compare like measurements. Runner CPU,
architecture, memory capacity, image, kernel, Docker, Python and Rust versions
are recorded. Exact runner image versions remain in the raw record. Shared
runner load still varies; the paired comparison is the regression gate.
An absolute history chart alone does not establish that a code change caused
a slowdown. Investigate failed comparisons and use repeat runs to assess noise.

The `Speed` workflow's manual input accepts a full ancestor SHA as a baseline.
Select the same main commit for both sides to run an A/A noise control.
The result still records both source identities and all measurements.
The history workflow also accepts an official run ID and attempt to retry
publication while its artifact is available.

On an isolated Linux Docker host, build both revisions with
`scripts/ci/build-linux.sh`, then run:

```sh
sh scripts/perf-run.sh /path/to/base-bin /path/to/head-bin /tmp/asagiri-perf-results 400
```

The script needs Python, Docker, `strace`, and root access to the isolated engine
sockets and process statistics. It removes only containers and volumes created
by that invocation. `results.json` retains raw v2 timing samples, `sizes.json`
records binary sizes, and `report.md` records the paired comparison.

These checks do not measure native Mac VM startup, Rosetta, WSL, cold image
downloads, or representative application workloads. The separate
[layer concurrency benchmark](layer-transfer-performance.md) and
[bind-mount report](bind-mount-performance.md) describe those narrower
experiments and their limits; they are not included in this history series.
