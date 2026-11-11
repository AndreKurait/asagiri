# Upstream work

Asagiri is an independent fork of [Kiri](https://github.com/libracoder/kiri).
The original Apache-2.0 license, copyright notices, and Git history are preserved.
The rename does not change the authorship of inherited code.

Upstream `main` was fetched on **2026-11-11** at
`a649319b5338de95f722a335d8f356406cb73c13`. The exact fetched revision is
recorded in the integration history; see the synchronization record below.

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
Earlier reviewed contributions are part of that history as well.

## Follow-on integration

The console-log, registry-mirror, Intel emulation, container-marker, and
stop/sleep follow-ups are tracked here as their ports complete. The signed
commit trailers are the authoritative mapping from the renamed source to each
upstream patch.

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
