# Upstream work

Asagiri is an independent fork of [Kiri](https://github.com/libracoder/kiri).
The original Apache-2.0 license, copyright notices and contributor authorship are preserved.

The frozen public inventory through **2026-11-18 14:50 UTC** covers
[280 pull requests](docs/upstream-pr-audit.md): 136 merged, 70 closed without
merging and 74 open. The [issue inventory](docs/issue-verification.md) maps
all 231 reports to checked-in checks or explicit remaining gaps. The records
retain upstream state, source identities, patch hashes and previous reviews.
All 70 new issue bodies and 134 changed discussion comments were read.
PR rows distinguish completed source review from implementation review that
remains open.

The [linked verification work](docs/upstream-audit-tasks.md) contains 176
cases published in this fork, plus the rustix advisory found during source
review. Related reports share a task where recorded. Source and execution
evidence are required before closing a task; native-platform and design
gaps stay open.

The current integration adds production-bound health, HTTP parser and layer
admission proofs, cancellable layer waiters, fail-safe container metadata,
TCP/file-read fixes and five Docker API compatibility ports. Local baseline
failures and candidate checks are preserved; complete platform, live API and
release qualification remains separate. Stable v0.1.13 already supplies the
built-in Docker-driver builder and rootfs image import.

The [engine comparison](docs/engine-comparison.md) includes the unfavorable
cleanup p95 that motivated the removed-cgroup regression checks.
[Verus verification](docs/verified-ipam.md) covers the production IPAM gap
search, with further [runtime contracts](docs/verified-runtime.md). These
function proofs do not certify the complete engine. Prior release and
component evidence remain in `docs/verification/` and the audit JSON records.

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
| #100 | Brazilian Portuguese guides, including the signed language-list, UI-label and kernel-patch follow-up |
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
| #54 through `508ba7e` | Automatic recovery for running `always`/`unless-stopped` containers, backoff, cancellation and confirmed VM retirement. Asagiri also waits for old-reader exit accounting, cancels and joins Linux listener threads, and preserves stop/remove responses and startup exit codes. Private Linux tests crash the agent and verify persisted recovery without API-triggered boots. |
| #116: `783de87`, `ba7fd94` | Intel macOS arm64 emulation and registry-owned remote tags. The emulator share is read-only and the platform exception is limited to Linux arm64. Asagiri records local tag provenance so local `FROM` tags in the same repository still work; OCI export checks the requested architecture's ELF. Release packaging builds a pinned static QEMU with its complete corresponding source and verifies ARM64 execution on a native Linux x86_64 runner. Intel Mac VM execution remains unverified. |
| #131: `11f698a` | A container's own changes to `/.dockerenv` remain visible in diff and commit. |
| #142 reviewed stop/sleep follow-up | Stop waits for the run it was asked to stop; sleep attempts every container before returning its first error. |

The signed commit trailers map each renamed port to its source commit and
preserve its original author. The extra correctness fixes are separate commits.

The latest main merge also retains Arabic and right-to-left UI support (#182),
kernel discovery on PATH (#174), port-owner reporting (#176), macOS mount
resolution (#178), the Stop/Start daemon lifecycle (#181), startup error details
and Docker exit codes (#183), Linux network cleanup (#184), macOS terminal
resize retries (#185), and the once-per-failure NAT warning (#186).

The final open-PR scan also includes #177's keyboard-copy fix (`ee05b04`) and
#119's upstream Arabic wording (`d21bbad`) and literal-string UI checks
(`cd0a5ab`). Its `b4092bc` main merge is covered by the full main integration.
The mirror credential fix in `fc739e4` is already enforced in Asagiri's
`download_layers`, with a test checking the complete headers for the manifest,
config, and layer requests. The atomic firewall update in #60 (`6454f41`) is
also present: `replace_table` sends create/delete/rebuild in one batch, with
tests for IPv4 and IPv6. The existing bridge-isolation rules are preserved.

The subsequent refresh ports #192 (`6d60e28`): `/opt` and `/usr/local` use
Mac host files with component-boundary matching, and the Mac virtiofs share
gets best-effort 1 MiB read-ahead. Other filesystems keep their settings.
The measurement helper and reproduction notes are included; upstream's
Mac throughput measurements have not been repeated on Asagiri.
Its later benchmark and percentile corrections (`d26ad2b`, `d972966`)
are also ported, with the original helper hash and Kiri/Colima results
preserved. The documentation explicitly distinguishes those upstream
measurements from new Asagiri measurements.

The full open-and-closed review also covers #193 (`8d6368b`), already
represented by Asagiri's broader #163 implementation. The live test retains
IPv4/IPv6 defaults, restart behavior and failover without duplicate routes,
and now verifies that two internal networks have no default route before or
after a restart.

The November 14 refresh brings the UI to 38 languages (#200/#205), adds
the searchable first-start language list (#219) and short sidebar labels (#220),
and includes configurable container address pools (#217). It also ports the
macOS 15 Rosetta kernel fix (#208), automatic USB reattachment (#212), canceled
restart handling (#216), and #207's share/API/agent hardening. Asagiri additionally
serializes settings writes, cancels stale USB reconnect tokens without waking
the VM, and releases connection slots when a thread cannot start.

The next scan reviewed #231–#236. Linked Mac bind folders are resolved again
on each start (#231), waiting USB devices remain visible and cancelable even
after a quick drop (#233), and engine Start/Stop transitions preserve held
restart policies while stopping containers in parallel (#234). Failed
executables now report Docker-compatible exit codes and messages. Asagiri's
lifecycle locks, canceled-restart generations and automatic crash recovery
remain intact. Its broader interface reservation code already covers #232;
all seven live scenarios pass with Alpine and BusyBox.

The VM card fits all 38 languages (#235), and the first-start language picker
is shared with Windows setup, with a searchable Settings menu (#236).
Browser checks cover keyboard and IME input, compact windows, 125% text
scaling, and truthful unknown Windows memory readings. Linux source `44fab19`
passes 569 unit tests, the new power and executable regressions, three actual
agent crashes, and stopped-daemon restart/cancellation checks. Pure Mac USB
state and bind-path tests pass; native Mac VM, physical USB and Windows WSL
execution remain separate validation work.

Replacement #238 has the same VM-card runtime patch as #235. Its new
word-boundary test uses `Intl.Segmenter` for Thai, Lao, Khmer and Burmese,
where spaces do not reliably separate words. That test is ported; the closed
#219, #220 and #235 source heads and their earlier ports remain in the audit.

The changed #122 head is reviewed through `944e72a`: its rebased initial trim
implementation is already inherited, and its new worker-process fix is ported.
The agent stays single-threaded for container cloning, while the trim child
closes inherited descriptors and is reaped on exit. A standalone Linux test
verifies that behavior without a VM or real data disk. The actual macOS sparse
disk reclamation remains a platform validation gap.

The November 15 scan reviews #240–#244. Image lists expose labels and support
label filters (#241), including images indexed before labels were cached.
Successful archive uploads reuse their HTTP connection after the entire framed
body has been consumed (#243). Path decoding and atomic index recovery are
already covered; the stricter malformed-input and backup requirements remain.
Kiri's #244 release notes do not change Asagiri's version or release claims.

New comments on #227/#230 identify OverlayFS metadata-only copy-up and directory
redirects as another source of incomplete commit layers. Asagiri explicitly
disables those features and rejects older incomplete upper entries before
diff/commit succeeds. The index overlap protection, archive-worker completion
barrier and daemon mount recovery remain in place. The subsequent systemd
cleanup workaround is covered by Asagiri's own startup recovery; it does not
require installing an extra unmount script.

Creation now reads and parses the image config once. A private Linux comparison
of `5721ace` and runtime `749c40c` measured 39.6% faster median create/remove
with a large image config and 15.8% faster median upload-plus-ping across
400 pairs. The [performance notes](docs/performance.md) retain the raw samples
and measurement limits. These workloads also run in the versioned GHA history.

## Issue follow-up

| Issue | Work and verification scope |
| --- | --- |
| #85 | Implemented streamed `docker import` from a tar/gzip file, stdin, or HTTP(S) source. Live tests cover metadata, files and links, uploads over 8 MiB, chunked bodies, execution, save/load, and rejected input. See [docker import](docs/docker-import.md). |
| #83 | `tests/buildx-bootstrap.py` runs six fresh initial bootstraps, records a failure before trying recovery, and builds, loads, and runs an image through the managed builder. A bounded passing run does not prove the intermittent reset cannot recur. |
| #93 | #61 bridge recovery now restores daemon availability without waking a stopped engine; background/window startup shares that rule. Managed Buildx registration and saved preferences are restored only for a confirmed running engine. Serialized startup and bounded background retries have pure regressions. Native Windows sleep and WSL restart remain unverified. |
| #48 | Linux tests cover bind-mount inotify, default and custom seccomp, Buildx, and container persistence after restarting the daemon. These do not verify macOS updater/launchd behavior or host notifications through Virtualization.framework virtiofs. |
| #50 | Guest console retention and #54 recovery are included. Private Linux agent-crash tests cover automatic recovery, policies, counters and writable layers. The reported macOS guest-kernel crash and recovery under its original load remain unverified. |
| #168 | WebView2 suspend/unload behavior remains unverified and unchanged. |
| #73, #74 | Implemented an explicitly enabled, password-protected loopback browser and a local stdio MCP server with bounded read tools and separate control permission. Real private Linux engine/Chromium checks and official MCP Python SDK 2.3.0 checks pass. Native Mac and Windows named-pipe execution remain unverified. The public website demo continues to use sample data. |
| #226 | Each five-workload GHA comparison retains 6,000 raw samples and 15 median/p95 results for Asagiri, Docker 29.9.3 and Podman 6.1.3; exact-commit success gates every main release. The per-commit performance workflow tracks timings, memory and syscalls with a public history. Native Mac/Colima VM comparisons remain unmeasured. See [engine comparison](docs/engine-comparison.md). |
| #230 | Private Linux tests restart the actual daemon in a persistent mount namespace and recover four orphaned mount layers after a crash, preserving writable data. They also verify Stop/Start policy retention and user cancellation across a daemon restart. The reporter's exact CachyOS kernel/systemd combination remains unverified. |
| #245 | The reported `PG_VERSION` named-volume copy already works. A live regression now checks a volume populated by one container and read by a new container while created and after exit. Native Mac execution of Supabase CLI 3 remains unverified. |

## Work intentionally held

| Upstream work | Reason |
| --- | --- |
| #89 prototype | Superseded by Asagiri's tested BuildKit bridge with bounded sessions, local image loading, explicit exports and cancellation. The earlier incomplete buffering and stream-termination implementation is not adopted. |
| #55 UI prototype | Its image metadata is already preserved by Asagiri's full-config inspect. The UI forces an extra dialog, guesses one port, reuses colliding default names and predates localization; existing rename/port editors and Docker create options remain available. |

#69's remaining registry configuration behavior is implemented with atomic
`daemon.json` updates, explicit HTTP/self-signed authority policy, bounded
transfer workers and credential/redirect boundaries. Private Linux tests
exercise real pull, push and BuildKit builds; see the
[registry configuration guide](docs/registry-configuration.md).

Intel macOS emulation and Windows WebView2 lifecycle behavior need their
respective hardware and operating systems for runtime verification. A passing
cross-platform compile is not evidence that those runtime paths were exercised.
The real macOS VM recovery test remains opt-in; synthetic unit tests never boot
a VM, and the automated crash regression runs only in the disposable Linux
test engine.
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

The later November 15 scan adds #249 (`978655fba390`): native x86_64 and
aarch64 AppImages with the GTK window and static engine. The adapted installer
preserves existing services and different executables. Packaging pins its
tools, includes runtime sources and licenses, and checks CLI extraction and
a headless GTK window in CI. Host desktop libraries and Docker CLI tools remain
separate dependencies; desktop password prompts and FUSE mounting are not
covered by the headless check.
