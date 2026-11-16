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
changes the harness. Main publication requires a successful comparison for
the same commit, alongside the other required checks. Its completion can
automatically resume a release waiting for checks. Incorrect workload results
or failed cleanup fail the comparison; relative Docker/Podman timings have no
pass/fail threshold. Each run retains its JSON, report and engine logs as a
GitHub artifact for 90 days. Asagiri changes also have the separate
[performance comparison against the previous commit](performance.md), with
regression gates and permanent raw history.

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
