# Compare container engines

`scripts/reference-bench.py` measures Docker-compatible Unix sockets using the
same pinned Alpine image, platform and unpacked layers. It accepts Asagiri,
Docker, Colima's Docker socket and Podman's compatibility socket. It creates
short-lived containers labeled with a unique run ID, and removes only those
containers. Images remain in a user-supplied engine's cache.

The `Engine comparison` GitHub Actions workflow builds the current Asagiri
commit and starts three separate, disposable Linux engines: Asagiri, Docker and
Podman. Their images are pinned in `scripts/ci/reference-images.json`. Each has
its own storage and private Unix socket, with no published host ports. Podman
uses its rootful service, cgroupfs and kernel overlay storage. Docker uses the
pinned image's default storage configuration; the report records what it used.

The workflow runs weekly, manually, and when the harness changes on main or a
pull request. Each run retains its JSON, report and engine logs as a GitHub
artifact for 90 days. Ordinary Asagiri changes also have the separate
[performance comparison against the previous commit](performance.md), with
regression gates and permanent raw history.

## Workload

Ten excluded warmup lifecycles run on each engine. Then 400 rounds rotate and
reverse engine order to reduce ordering effects. Each round records:

- An API ping over a persistent Unix HTTP connection.
- A container list filtered to the current run.
- A create, start, wait and remove lifecycle running `true`, with networking
  disabled.

All individual timings, median and nearest-rank p95 are retained. The JSON
records the source commit, engine versions, image digests and layer identity,
binary and harness hashes, storage settings, reported CPU/memory, runner image
and host details. Pulling images and starting engines happen before measurement.

This is an API and short-container workload. It does not measure VM startup,
idle VM memory, application throughput, builds, bind mounts or network speed.
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
engine settings or global binfmt registrations.

For the isolated Linux workflow, compile with `scripts/ci/build-linux.sh` and
run `scripts/ci/reference-run.py --binaries /path/to/binaries --out /new/output`.
The orchestrator requires Docker and root access to its private test sockets.
It verifies the compiled source commit and checksums, and removes its own
engine containers and anonymous stores after the comparison.
