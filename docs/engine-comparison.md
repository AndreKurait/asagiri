# Compare container engines

`scripts/reference-bench.py` measures Docker-compatible Unix sockets using the
same pinned Alpine image, platform and unpacked layers. It accepts Asagiri,
Docker, Colima's Docker socket and Podman's compatibility socket. It creates
containers and a named volume labeled with a unique run ID. Cleanup checks
ownership and verifies their removal through the API. Images remain in a
user-supplied engine's cache.

The `Engine comparison` GitHub Actions workflow builds the current Asagiri
commit and starts three separate, disposable Linux engines: Asagiri, Docker and
Podman. Their images are pinned in `scripts/ci/reference-images.json`. Each has
its own storage and private Unix socket, with no published host ports. Podman
uses its rootful service, cgroupfs and kernel overlay storage. Docker uses the
pinned image's default storage configuration; the report records what it used.
Podman's pin uses its versioned `-immutable` image: the previous daily rebuilt
`stable` manifest disappeared from the registry and caused run
`43814793620` to fail before measurement. Image references still include their
SHA-256 digest; an unavailable pin fails the run instead of silently changing
the reference engine.

The workflow runs on every main push, weekly, manually, and when a pull request
changes the engine, build inputs or harness. Main publication requires a successful comparison for
the same commit, alongside the other required checks. Its completion can
automatically resume a release waiting for checks. Incorrect workload results
or failed cleanup fail the comparison; relative Docker/Podman timings have no
pass/fail threshold. Each run retains its JSON, report and engine logs as a
GitHub artifact for 90 days. Asagiri changes also have the separate
[performance comparison against the previous commit](performance.md), with
regression gates and permanent raw history.

## Investigated lifecycle cleanup regression (workload v2)

[GHA run 43878562486](https://github.com/AndreKurait/asagiri/actions/runs/43878562486)
measured PR55 head `ed482bd6e03354441e0396172d42bd175ab271e8` through source
`5b540a35b8371296768fb24fc5f56a6195ef26c3`, with an identical source tree.
This was a source-built Linux x86-64 CI binary reporting `0.0.0`. Its runner
reported four CPUs, AMD EPYC 7763, Linux `7.0.0-1017-azure`, and Ubuntu
image `20261111.370.1`. The comparison passed independently; that revision's
separate platform suite failed on an oversized seccomp test fixture.

Each cell is **median / p95 in milliseconds**, from 400 samples:

| Workload | Asagiri `ed482bd` | Docker 29.9.3 | Podman 6.1.3 |
| --- | ---: | ---: | ---: |
| Ping | 0.204 / 0.259 | 0.396 / 0.460 | 0.396 / 0.476 |
| Empty filtered list | 0.191 / 0.239 | 0.301 / 0.354 | 0.389 / 0.457 |
| Create/start/wait/remove | 7.887 / 259.158 | 116.077 / 122.916 | 40.091 / 46.997 |
| Exec, output and exit check | 1.968 / 2.177 | 37.962 / 39.699 | 54.052 / 66.783 |
| 64 KiB named-volume file round trip | 1.754 / 1.876 | 3.984 / 4.700 | 194.222 / 201.386 |

The [raw record](benchmarks/engine-comparison-2026-11-16-ed482.json) retains
all 6,000 samples. Its
[independent verification](benchmarks/engine-comparison-2026-11-16-ed482-verification.json)
recomputes the medians and p95s and binds the source, artifact, image layers,
payload and recorded fixture cleanup. As with earlier ordinary comparisons,
the artifact records executable hashes but does not retain the executable
bytes for an independent rehash.

Asagiri had 32 of 400 measured lifecycle operations exceed 240 ms; Docker
and Podman had none. Retained engine events placed the long interval after
exit and before destruction in all 32 cases. The corresponding median
interval was about 253 ms. Those timestamps locate the delay but do not
alone establish its cause.

The investigation found that an open, removed cgroup directory can retain
a nonzero link count. The removed-directory regression tests fail on the
old implementation and pass when removal is checked through the pinned
directory's `cgroup.procs` entry. A replacement at the old pathname remains
untouched. The first local candidate build incorrectly reused the baseline
test executable; that result was discarded. A separate cache and checked
executable hashes established that the corrected candidate was actually
compiled before its 15 cgroup tests passed on Linux 6.18.58/aarch64.
This is a targeted correctness result; a full-engine comparison is needed
to measure its effect on the slow lifecycle samples.

## Compare revisions on the same machine

The manual `Engine comparison` workflow accepts an optional `baseline`
input containing a full ancestor commit SHA. Select the candidate branch
as the workflow ref. Both revisions must use the same workload, runner,
reference-image pins and build recipe.

The job compiles both sources, then runs the existing three-engine
comparison in baseline/candidate/candidate/baseline order. It retains
24,000 raw samples, build logs and executable bytes, with 800 measurements
per version, engine and operation. The report includes medians, p95,
individual-leg medians and the count of lifecycle samples over 240 ms.
Source and executable hashes identify every leg; the reader rejects
mismatched hosts, inputs, incomplete samples and replayed legs.

Passing establishes measurement integrity, not a speedup. This order
reduces simple time-order bias; it does not establish performance on other
machines or application workloads. The ordinary per-commit comparison
continues to run independently.

## Earlier PR50 Linux result (workload v2)

[GHA run 43831181768](https://github.com/AndreKurait/asagiri/actions/runs/43831181768)
measured PR50 head `4377ea5c96f0d589501ee1c0096ee1be49baa6c8` through tested source
`2aadf0273a19552b3d4b4b3bc8a60cd9fa7a7196`. Its tree equals merged main `77444fd1239b9dda5049b234a5053fbb2958994b`.
The source-built CI binary reports `0.0.0`; this is not an installed-release
measurement. The runner reported four CPUs, AMD EPYC 9V74, Linux
`7.0.0-1017-azure` and Ubuntu image `20261111.370.1`.

Each cell is **median / p95 in milliseconds**, from 400 samples:

| Workload | Asagiri `4377ea5` | Docker 29.9.3 | Podman 6.1.3 |
| --- | ---: | ---: | ---: |
| Ping | 0.171 / 0.224 | 0.363 / 0.425 | 0.351 / 0.394 |
| Empty filtered list | 0.141 / 0.196 | 0.250 / 0.305 | 0.339 / 0.386 |
| Create/start/wait/remove | 6.394 / 14.508 | 100.798 / 250.753 | 35.763 / 88.930 |
| Exec, output and exit check | 1.642 / 1.786 | 31.379 / 32.514 | 65.019 / 284.594 |
| 64 KiB named-volume file round trip | 1.418 / 1.491 | 3.247 / 3.899 | 157.183 / 161.435 |

The [raw record](benchmarks/engine-comparison-2026-11-16-4377.json) retains all
6,000 samples. Its [independent verification](benchmarks/engine-comparison-2026-11-16-4377-verification.json)
recomputes all 15 medians/p95s and checks artifact/source identity, matching
platform and unpacked layers, payload bytes and recorded fixture cleanup.
Executable hashes were checked by CI; the artifact does not include executable
bytes for a separate local rehash.

Dev.100's main [run 43827712984](https://github.com/AndreKurait/asagiri/actions/runs/43827712984)
also passed, on an Intel Xeon 6975P-C/AWS host. It used a different host from
PR50; comparing their timings does not isolate a code change. None of these
results qualifies the later followup candidate, native Mac/Windows VMs or
general application throughput.

## Earlier PR49 Linux result (workload v2)

[GHA run 43823750794](https://github.com/AndreKurait/asagiri/actions/runs/43823750794)
measured PR #49 head `c610a2173463a35e3e3eac329d93b7c63b90c41b` through
merge source `48a1c0c4cc6149e842f3054d780f60723c95bc83`. Their trees are
identical. The CI binary reports product version `0.0.0`; this is a measurement
of that source, not an installed release. The runner reported four CPUs,
AMD EPYC 9V74 and Linux `7.0.0-1017-azure`, with Ubuntu runner image
`20261111.370.1`.

Every cell is **median / p95 in milliseconds**, from 400 samples:

| Workload | Asagiri `c610a21` | Docker 29.9.3 | Podman 6.1.3 |
| --- | ---: | ---: | ---: |
| Ping | 0.181 / 0.239 | 0.378 / 0.464 | 0.368 / 0.447 |
| Empty filtered list | 0.145 / 0.191 | 0.271 / 0.356 | 0.363 / 0.433 |
| Create/start/wait/remove | 7.532 / 26.414 | 112.668 / 259.414 | 42.884 / 158.142 |
| Exec, output and exit check | 1.762 / 1.990 | 32.454 / 33.963 | 57.603 / 222.099 |
| 64 KiB named-volume file round trip | 1.229 / 1.323 | 3.552 / 4.299 | 161.899 / 172.091 |

The [raw record](benchmarks/engine-comparison-2026-11-16-c610.json) retains
all 6,000 samples. The
[verification record](benchmarks/engine-comparison-2026-11-16-c610-verification.json)
checks the GitHub artifact digest, source tree, harness and image pins,
independently recalculates all 15 medians and p95s, and checks matching image
layers, payload bytes and recorded cleanup. Compiled binary hashes were
checked and recorded by the workflow; its comparison artifact does not
include the executable bytes for an independent local rehash.

The runs below are retained with their own source identities. Shared runner
load varies, and no controlled experiment attributes timing differences
between these runs to a code change. This five-workload Linux comparison
does not establish native Mac VM, Windows WSL or application performance.

## Earlier Linux result (workload v2)

[GHA run 43816520898](https://github.com/AndreKurait/asagiri/actions/runs/43816520898)
completed on November 16, 2026, with 400 samples per workload and engine.
Asagiri was built from `2214570049b2370f2f41761e827d5690ecc4cb7c`; its engine
sources and build settings match main `6e4e2ca7029f2769972b936708df3986c38990ae`.
This CI build reports product version `0.0.0`, so the commit identifies it.
The runner reported four CPUs, an AMD EPYC 9V74 and Linux
`7.0.0-1017-azure` (`ubuntu24`, image `20261111.370.1`).

Every cell below is **median / p95 in milliseconds**:

| Workload | Asagiri `2214570` | Docker 29.9.3 | Podman 6.1.3 |
| --- | ---: | ---: | ---: |
| Ping | 0.163 / 0.203 | 0.349 / 0.404 | 0.345 / 0.394 |
| Empty filtered list | 0.134 / 0.172 | 0.249 / 0.297 | 0.338 / 0.392 |
| Create/start/wait/remove | 6.730 / 9.089 | 98.022 / 264.104 | 34.900 / 83.282 |
| Exec, output and exit check | 1.704 / 1.856 | 32.008 / 33.218 | 55.407 / 160.464 |
| 64 KiB named-volume file round trip | 1.202 / 1.300 | 3.372 / 3.867 | 159.786 / 164.787 |

The [raw record](benchmarks/engine-comparison-2026-11-16-v2.json) retains all
6,000 samples. Its
[independent verification](benchmarks/engine-comparison-2026-11-16-v2-verification.json)
checks the GitHub artifact digest, exact source and harness identities,
recalculates all 15 medians and p95s, and checks the identical image layers,
file payload hash and recorded fixture cleanup.

All engines reported four CPUs and the same available memory. Docker used
its containerd overlay snapshotter; Podman used kernel overlay with native
overlay diff disabled; Asagiri used overlayfs. The raw record keeps those
storage differences. These are observations from one hosted runner. Different
runner hardware and the changed workload prevent treating differences from
the historical v1 table as an Asagiri performance regression or improvement.

## Historical Linux result (workload v1)

[GHA run 43632761522](https://github.com/AndreKurait/asagiri/actions/runs/43632761522)
completed on November 14, 2026, with 400 rounds per engine. The runner had four
logical CPUs, an Intel Xeon 6975P-C and Linux `7.0.0-1014-aws`
(`ubuntu24`, image `20261111.127.1`). The Asagiri source tree was
`a50dc8f6bfdbdc73f3a4e36f09ad88d9303ccdf8`; GHA checked out the equivalent
PR merge commit `05a43f1c20331815acaecb440c78e895326b55f4`.

| Engine | Lifecycle median / p95 | Ping median / p95 | List median / p95 |
| --- | ---: | ---: | ---: |
| Asagiri `a50dc8f` | 3.879 / 4.864 ms | 0.135 / 0.205 ms | 0.103 / 0.158 ms |
| Docker 29.9.3 | 69.308 / 75.475 ms | 0.219 / 0.307 ms | 0.168 / 0.261 ms |
| Podman 6.1.3 | 21.429 / 24.369 ms | 0.230 / 0.302 ms | 0.223 / 0.321 ms |

The [complete raw record](benchmarks/engine-comparison-2026-11-14.json) preserves
all 3,600 timings, image identities and settings. The
[verification record](benchmarks/engine-comparison-2026-11-14-verification.json)
records the checked GitHub artifact digest, source-tree identity and independent
recalculation of every median and p95. These files remain in Git after the
workflow artifact expires.

Docker used its containerd overlay snapshotter, Podman used kernel overlay,
and Asagiri used overlayfs. No per-engine CPU quota was requested. Asagiri's
`/info` reported `NCPU: 2` in this revision; the host and other engines reported
four, and the raw record retains that difference. Podman 6.1.3 is the version
actually present in the pinned official container image.

These numbers describe one same-runner API workload, not overall application
performance. No Colima or native Mac VM result was measured. This historical
record uses `docker-api-reference-v1`; it contains no exec or file-copy results
and is not a measurement of the current source or workload v2.

## Workload

The current JSON uses schema 2 and `docker-api-reference-v2`. It measures five
workloads in two phases. The first phase contains no long-running fixture:

- An API ping over Unix HTTP, reusing connections where the protocol permits.
- An empty container list filtered to the current run.
- A create, start, wait and remove lifecycle running `true`, with networking
  disabled.

The second phase starts one Alpine container per engine with a plain local
named volume and networking disabled. It measures:

- Creating and starting an exec, reading its expected output, and inspecting
  its successful exit.
- Uploading and downloading a 64 KiB file through the named volume, including
  client archive parsing and SHA-256 verification of the returned bytes.

Each phase excludes ten warmup rounds and a three-second settling pause, then
records 400 rounds that rotate and reverse engine order. Container/volume setup
is outside timing. The original empty-list metric is measured before the
second phase creates its service fixtures.

All individual timings, median and nearest-rank p95 are retained. The JSON
records the source commit, engine versions, image digests and layer identity,
binary and harness hashes, storage settings, reported CPU/memory, runner image
and host details, plus the file payload hash and verified fixture cleanup.
Pulling images and starting engines happen before measurement.

This measures API latency, short containers, exec and a small file round trip.
It does not measure VM startup, idle VM memory, application throughput, builds,
bind mounts, disk throughput or network speed.
Linux hosted results cannot establish Mac, Colima VM, Rosetta or WSL performance.
Shared runner load also varies; reproduce a result before generalizing it.

## Use existing local endpoints

Start the engines through their normal setup and obtain their actual Unix
socket paths. Pass only engines on the same host and architecture:

```sh
python3 scripts/reference-bench.py \
  --engine "Asagiri=unix:///absolute/path/to/asagiri.sock" \
  --engine "Colima=unix:///absolute/path/to/colima/docker.sock" \
  --engine "Podman=unix:///absolute/path/to/podman.sock" \
  --runs 400 --out /tmp/asagiri-engine-comparison
```

The output directory must be new. The script uses Python's standard library,
requires permission to access each socket, and fails if the engines resolve the
image to different architectures or layers. It never changes Docker contexts,
engine settings or global binfmt registrations. Add `--no-pull` when every
engine already has the exact pinned image; a missing image then fails locally.

For the isolated Linux workflow, compile with `scripts/ci/build-linux.sh` and
run `scripts/ci/reference-run.py --binaries /path/to/binaries --out /new/output`.
The orchestrator requires Docker and root access to its private test sockets.
It verifies the compiled source commit and checksums, and removes its own
engine containers and anonymous stores after the comparison.
