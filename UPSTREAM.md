# Upstream work

Asagiri is an independent fork of [Kiri](https://github.com/libracoder/kiri).
The original Apache-2.0 license, copyright notices, and Git history are preserved.
The rename does not change the authorship of inherited code.

Upstream `main` was fetched on **2026-11-12** at
`4645536f68d4bf2f5e708bd8339a9f71f844c43a`. Its complete history is merged,
including the nine commits after the initial `a649319` integration.

## Integration policy

We fetch upstream `main` before each integration and again before merging a
release branch. Existing public history is kept. After the rename, an upstream
patch is compared across two mechanically renamed trees and applied with a
three-way merge. Conflicts are reviewed, never resolved by discarding the
upstream tree wholesale.

Each port records its upstream commit and original author. Fork-specific
follow-ups are described separately. Tests run against the resulting Asagiri
tree, including the platform builds and private Linux engine tests in CI.
Upstream PRs can be unfinished; an open PR is not a promise that its entire
branch belongs in a release.

## Integrated before the rename

The initial integration carries the complete upstream main history plus the
reviewed changes from these upstream pull requests:

| Upstream PR | Change |
| --- | --- |
| #16 | cargo-deny, ShellCheck, zizmor, and OpenSSF Scorecard checks |
| #60 | nftables networking on Linux and WSL kernels before 6.3 |
| #61 | Restarting the engine after WSL goes away |
| #100 | Brazilian Portuguese README, features, and development guides |
| #122 | Reclaiming unused blocks from the macOS VM data disk |
| #124 | Correct handling of an empty command array |
| #171 | Linux WebKitGTK hover rendering |
| #172 | NetworkManager exclusions for engine interfaces |
| #173 | Stopping the engine without a launchd respawn loop |
| #174 | Finding the guest kernel from a command on PATH |
| #175 | macOS `/etc` mounts and linked mount sources |
| #176 | Identifying the program holding an allocated port |
| #177 | Selecting and copying log text |

The merge commits preserve the source commits and identify their authors.
The public compatibility contributions are included too:

| Area | Upstream PRs |
| --- | --- |
| Container lifecycle, exit status, and attach | #126, #139, #141, #142, #143, #149, #166, #169 |
| Archives, paths, permissions, and container markers | #131, #132, #133, #135, #136, #137, #153 |
| API versions, resource limits, and seccomp | #138, #140, #154, #158 |
| Bridge, DNS, routes, interfaces, macvlan, and ipvlan | #150, #151, #152, #157, #160, #162, #163, #164, #165 |
| Performance runner socket selection | #159 |

The newer #137 commits (`781b8fd`, `4a038f2`) were checked through the port
script and are already represented. The rebased #163 commits (`e087e2b`,
`ec6afd9`) are also present by content, including their live tests; later
macvlan, ipvlan, DNS, and MTU work is retained around them. The #159 socket
fix is preserved in the performance runner's flavor-dependent socket setting.

## Follow-on integration

| Upstream work | Included changes and Asagiri follow-ups |
| --- | --- |
| #53: `ca8a6e5`, `99ea574`, `968cf67` | Bounded guest console recording, rotation failure handling, and documentation. The key-redaction latch also follows delimiters across reads and beyond an omitted line's cap. |
| #119: `a0b8d90`, `79ee480`, `4ac9cdd` | Registry mirrors, translated settings, Windows setup continuation, and neutral examples. Origin credentials are kept off mirror manifests and blobs; refreshing settings preserves active Buildx execs. Windows setup treats pull failures separately from container execution failures. |
| #116: `783de87` | Intel macOS arm64 emulation. The emulator share is read-only, the platform exception is limited to Linux arm64, and QEMU's license and source information accompany the bundle. |
| #131: `11f698a` | A container's own changes to `/.dockerenv` remain visible in diff and commit. |
| #142 reviewed stop/sleep follow-up | Stop waits for the run it was asked to stop; sleep attempts every container before returning its first error. |

The signed commit trailers map each renamed port to its source commit and
preserve its original author. The extra correctness fixes are separate commits.

The latest main merge also retains Arabic and right-to-left UI support (#182),
kernel discovery on PATH (#174), port-owner reporting (#176), macOS mount
resolution (#178), the Stop/Start daemon lifecycle (#181), startup error details
and Docker exit codes (#183), Linux network cleanup (#184), macOS terminal
resize retries (#185), and the once-per-failure NAT warning (#186).

## Issue follow-up

| Issue | Work and verification scope |
| --- | --- |
| #85 | Implemented streamed `docker import` from a tar/gzip file, stdin, or HTTP(S) source. Live tests cover metadata, files and links, uploads over 8 MiB, chunked bodies, execution, save/load, and rejected input. See [docker import](docs/docker-import.md). |
| #83 | `tests/buildx-bootstrap.py` runs six fresh initial bootstraps, records a failure before trying recovery, and builds, loads, and runs an image through the managed builder. A bounded passing run does not prove the intermittent reset cannot recur. |
| #93 | #61 is included: accepting a client obtains a live bridge, and losing a bridge that completed its handshake queues an engine start and logs the attempt. Windows suspend/resume still needs a Windows runtime test. |
| #48 | Linux tests cover bind-mount inotify, default and custom seccomp, Buildx, and container persistence after restarting the daemon. These do not verify macOS updater/launchd behavior or host notifications through Virtualization.framework virtiofs. |
| #50 | Guest console retention is included. The reported macOS guest-kernel crash and recovery under its original load remain unverified. |
| #168 | WebView2 suspend/unload behavior remains unverified and unchanged. |
| #73, #74 | Browser control of a real engine and an MCP server are not implemented. The website demo uses sample data in memory. |

## Work intentionally held

| Upstream work | Reason |
| --- | --- |
| #54 | Review found two deadlocks. Revisit when the synchronization design is fixed and covered by lifecycle tests. |
| #89 | Incomplete wiring, a protocol change, and unresolved correctness findings. It needs a complete implementation and compatibility review before integration. |
| #116, commit `ba7fd94` | Kept separate from the initial emulation implementation pending review and platform validation. |

Intel macOS emulation and Windows WebView2 lifecycle behavior need their
respective hardware and operating systems for runtime verification. A passing
cross-platform compile is not evidence that those runtime paths were exercised.
Historical Kiri release notes and benchmark results remain explicitly labelled
as upstream material; they are not Asagiri releases or Asagiri measurements.

## Porting a commit

Start from a clean working tree and fetch the public source commit:

```sh
git fetch https://github.com/libracoder/kiri.git main
scripts/port-upstream.sh --no-commit <commit>
git diff --cached
# Resolve conflicts, run relevant checks, then commit with attribution.
git commit -S -s
```

Without `--no-commit`, the script creates a signed, signed-off commit preserving
the source author and adding `Upstream-Commit` and `Co-authored-by` trailers.
Port individual commits in their original order. The script refuses dirty
tracked state, does not push, and leaves conflicting changes in the index for
review. It imports only the requested public source trees.

`scripts/rebrand.py` handles package, binary, environment-variable, path, and
identifier names. Project documents, attribution, and historical material are
maintained by hand. Network endpoints, installer identities, platform limits,
and fork-specific behavior must still be reviewed after applying a patch.

## Synchronization record

See the signed integration commits on `main` and the corresponding Asagiri pull
requests for checks, source revisions, and any follow-up fixes.
