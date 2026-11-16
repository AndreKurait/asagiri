> Archived public upstream report from [Kiri PR #195](https://github.com/libracoder/kiri/pull/195), commit [`f059c1a5d9b83d4867eeba283f93072f86431212`](https://github.com/libracoder/kiri/commit/f059c1a5d9b83d4867eeba283f93072f86431212), by Supersynergy. The `custom` engine is the original author’s local fork. These observations are not Asagiri measurements. The raw export is unchanged. Four displayed medians are rounded directly from its samples: 18.10 → 18.09, 20.78 → 20.79, 1,287.28 → 1,287.27 and 52.05 → 52.04 ms.

# Official Kiri 0.2.1 versus an existing macOS fork

Measured on 16 November 2026. This is a follow-up to the earlier Kiri/Colima workflow report, not a replacement for those observations. It compares the official 0.2.1 app with an existing locally built fork containing the earlier bind/network work and later local changes. It does not isolate any one PR's effect.

## Method

Apple M4 Max, 128 GiB, macOS 15.8.2, Hyperfine 2.0.0. Two separate test daemons used private HOME, data and Unix-socket paths, two vCPUs and an 8 GiB ceiling each. Containers had two CPUs, a 256 MiB limit and no network. Both used the same ARM64 Alpine image, kernel 6.18.58 and static Linux reader. Binary, image and workload hashes are in the [data export](kiri-release-hyperfine-2026-11-16.json).

Each arm has 16 observations: two blocks of eight, one warmup per block, reversed command order in the second block. All 448 timed exits succeeded. The export retains each original measurement object unchanged, with block hashes; private command paths and local service identifiers are omitted. Wall times below include CLI and transport. Hyperfine's client CPU/RSS is not the whole VM's CPU/RSS.

## Release comparison

Median wall time, milliseconds; the same bundled Docker 29.9.2 client calls both engines directly.

| Workload | Existing fork | Official 0.2.1 |
|---|---:|---:|
| API ping, including curl startup | 8.16 | 7.88 |
| Run `true` and remove the container | 20.21 | 19.99 |
| Exec `true` in a running container | 18.09 | 17.97 |
| CPU workload, including exec | 61.08 | 61.20 |
| Stat 4,096 files | 21.19 | 20.79 |
| Read 4,096 small files on a Mac bind | 1,287.27 | 2,138.13 |
| Read the same files on a Linux volume | 27.65 | 36.32 |

The first five comparisons do not establish a release-speed advantage. Independent exploratory bootstrap intervals include parity. Bind-read block medians drifted from 939/976 ms to 1,364/2,421 ms (fork/official). Do not turn the pooled bind ratio into a general engine ranking. The fork's volume reads were faster in both blocks, but that remains this fixture, not all workloads.

The shared host's load average rose from 8.38 to 14.64. Available-memory percentage stayed at 89%; swap stock was unchanged. Host caches were warm. The official daemon's Finder view could not start because its isolated HOME path exceeded a socket-path limit; the fork's view started. This unbalanced ancillary service is another limitation. DNS port conflicts only disabled the test daemons' local-name servers. No UI, DNS, production migration or full compatibility claim follows.

## Three boundary-cost checks

1. **Client route.** A separate same-daemon probe controlled which Docker binary the wrapper used. With the same client, bypassing `kiri exec` reduced median wall time by roughly 26–36%. Normal wrapper: 52.04 ms; wrapper pinned to bundled Docker: 35.24 ms; system Docker directly: 33.23 ms; bundled Docker directly: 25.96 ms. System Docker was 29.8.2. These are invocation-route changes, not an engine patch. For buildx/Compose, preserve the wrapper's managed-builder selection rather than blindly bypassing it.
2. **Batching.** Ten separate execs versus one exec running ten `/bin/true` processes: 326.47 → 33.24 ms on the fork, 363.89 → 38.53 ms on official 0.2.1. This measures dispatch overhead, not ten real application tasks. Only batch operations whose identity, environment, ordering, exit handling and required outputs remain equivalent.
3. **Immutable dependency layout.** The fixture has 4,096 files of 256 bytes, each filled with byte `index % 251`. The Linux-volume copy was fully SHA-256 checked against the bind corpus. Per-block bind/volume ratios were 36.18–45.01× on the fork and 32.16–58.96× on official Kiri. Copy/rebuild time is excluded. Keep editable source coherent on host binds; only use separate volumes for immutable or correctly invalidated Linux dependencies/caches.

Do not multiply these gains. No combined production workflow was measured, and some savings remove the same costs. No RAM, balloon, security or kernel setting was changed.

## Reproduction outline

Use two private engines, identical images/resources and a common Docker binary. `hyperfine --warmup 1 --runs 8 --metrics time --export-json block-0.json ...` produces the first block; reverse command order for the second. Benchmark `run --rm --network none ... true`, `exec ... true`, and the same static reader on a read-only source bind versus a verified Linux copy. The reader source and its modes are documented in PR #192; reproduce the deterministic tree above. For batching, compare ten host-side exec calls with one guest-side `set -e` loop executing the same ten commands. Measure client startup separately using `docker --version` and the native `true` process floor.

All test containers, volumes and test daemons were removed/stopped. The installed fork and pre-existing containers were retained. A documentation-only follow-up reuses the completed measurements; it does not rerun them or claim fresh runtime-code verification.
