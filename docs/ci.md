# Continuous integration

Platform builds use Rust 1.99.1 on Linux, macOS, and Windows. Each platform
runs its tests and uploads development binaries with SHA-256 hashes. Linux
also runs the live Docker-compatible API tests inside a private test engine.
These artifacts are development builds, not signed product releases.

Linux PR/main checks and both Linux release architectures also run Java
Testcontainers 1.20.3 against a fresh engine. The check keeps startup checks
and Ryuk enabled, copies a file before container start, waits for two live
readiness messages, executes a command, starts pinned MySQL 8.0, and makes a
real JDBC query through its published port. A deliberately unclosed resource
must be removed after the JVM exits. The outer test host publishes no ports.
Maven artifacts are cached by the test POM and architecture; only successful
main checks save them, and Maven requires repository checksums.

The [Linux firewall probe](linux-firewall.md) runs on the platform job and both
Linux release architectures. Disposable hosts reproduce `FORWARD DROP` and
verify scoped TCP, UDP and DNS egress, NAT, inbound isolation and coexistence
with a pinned Docker daemon. Rules, source identities and logs are retained
for 90 days; the hosts and their private network are removed afterward.

The live suite also connects the official MCP Python SDK 2.3.0 to the stdio
server. It verifies protocol negotiation, schema parsing, default denial,
read tools, redaction, separately allowed actions and permission revocation.
Its Python dependencies are pinned with hashes and installed in a temporary
environment inside the disposable Linux host.

Dependency caches are tied to the compiler, platform, lockfile, manifests,
and Cargo configuration. Only pushes to `main` save caches; pull requests can
restore them. Workspace crates are rebuilt. The Linux helper removes their
artifacts before building and before saving the dependency cache, so a file
timestamp cannot cause the performance check to reuse the other revision's
engine or shared protocol library.

The Linux and speed jobs also cache the Rust, Docker CLI, and Alpine base
images through GitHub Actions. `scripts/ci/images.json` pins both the registry
manifest and each architecture's image content ID. After loading an archive,
the helper verifies that content ID before exporting the image for a build.
A warm cache needs no registry request. A missing, damaged, or incorrect cache
falls back to a pull by digest and is replaced only after verification.
Release jobs additionally cache their Debian, Ubuntu, Fedora, and Amazon Linux
2023 images.
Test engines, their data directories, and their pulled test images stay fresh.

The Linux live suite also runs Docker's actual BuildKit driver through
`/grpc` and `/session`: contexts, secrets, image loading, cache, file exports,
concurrent Bake builds, host-gateway, cancellation and builder preference
migration. These checks run for platform CI and both Linux release
architectures. They do not establish native Mac VM or Windows WSL behavior.

### Cache service choice

| Option | Fit for this repository |
| --- | --- |
| GitHub Actions cache | Selected. The runners already authenticate to it; PRs can restore main's caches without cloud credentials. No additional service to deploy. |
| sccache with the GitHub backend | Useful if compiler caching across differing dependency sets becomes necessary. The current dependency cache already avoids rebuilding unchanged dependencies; workspace engines are deliberately rebuilt for comparison. |
| ECR pull-through cache | Useful for an AWS runner fleet. It requires an ECR private registry and IAM access, adding cloud setup and credential handling to public contributor workflows. |
| CNCF Distribution or Harbor | Useful for a shared registry across several build systems. Operating the registry, its storage, access controls, and cleanup adds work this repository does not yet need. |
| Artifactory | Useful when an existing organization-wide artifact service is available. There is no need to add one solely for these jobs. |
| Custom API Gateway/Lambda/S3 cache | Not selected. It adds a registry protocol, authentication, integrity, eviction, and availability implementation to maintain. |

The implementation uses the existing managed cache and open-source cache
action. Images and compiler dependencies have separate keys so a lockfile
change does not download all the base images again. Image keys have no broad
fallback prefix. Only trusted main jobs save shared entries. Cache misses
affect build time, not which source or image is tested.

Linux compiler keys include the architecture, Rust version, dependency
manifests, pinned image manifest, and build script. Successful main release
jobs also save compiler dependencies after their native package tests, so
both x86_64 and Arm builds populate reusable caches.

Primary references: [Actions cache](https://github.com/actions/cache),
[sccache's GitHub backend](https://github.com/mozilla/sccache/blob/main/docs/GHA.md),
[ECR pull-through cache](https://docs.aws.amazon.com/AmazonECR/latest/userguide/pull-through-cache.html),
and [Distribution mirrors](https://distribution.github.io/distribution/recipes/mirror/).

The speed check compares a PR's merge result with its base, or a main push
with the previous main commit. It builds both revisions with the same pinned
compiler. If both engine binaries match byte for byte, the check reports that
equality and can skip a PR measurement. Main pushes and manual runs still
collect measurements. The alternating comparison uses the baseline's limits;
it does not silently substitute a retry for a failed measurement. Successful
and measured failing main runs retain raw data in signed commits on the
`performance-history` data branch. The public dashboard plots each source
commit and separates hardware and workload series. See
[performance tracking](performance.md) for metrics, gates, provenance,
retention and same-commit noise controls.
Only the main push's Speed run satisfies the release gate. Manual benchmark
experiments are archived, but cannot replace a failing required comparison or
block a passing one. They also use a separate `speed experiment` check name,
which cannot satisfy the PR's required `speed` check. Retry the original push
run to recheck its release gate.

The UI check runs the app's page tests and translation checks. The website
check also opens the assembled demo in Chrome and exercises navigation,
container actions, every settings tab, mirror edits, light/dark themes,
Arabic/English switching, and desktop/mobile layouts. Its check mode leaves
the committed images unchanged. Pages deploys only after those checks pass.
The deploy job then waits for `deployment.json` to name the source commit,
checks the public HTML and assets over HTTPS, and runs the interaction suite
again against the live project URL. Every push to main publishes automatically;
`workflow_dispatch` can retry a deployment without a source change.

The website's build and deployment use GitHub's `ubuntu-slim` runners; neither
needs Docker or a VM. Both run the same browser checks with a pinned Chrome
for Testing download. The cache stores only its original archive, whose size
and SHA-256 are checked before every extraction. The Python dependencies are
also hash pinned. This keeps browser checks reproducible without depending
on a preinstalled desktop browser or the full Linux build runner pool.
Failed, canceled, and otherwise ineligible release notifications use separate
concurrency groups, so they cannot displace a pending website deployment.

Security checks include dependency advisories, licenses and sources,
ShellCheck, workflow auditing, and CodeQL. Advisory failures remain failures
on PRs as well as main. Superseded PR checks are canceled. Automatic platform
and release builds cancel obsolete main runs to prioritize current source;
manual checks, stable tags and explicit release recovery retain their own
runs. Speed keeps every main run so performance history is not lost during
rapid pushes. Every main build that completes all required checks successfully
can publish its own prerelease.

Release packaging starts on each push to main, alongside those checks.
The prerelease version is `RELEASE_VERSION-dev.N`, where `N` is the Release
workflow's run number. A `vMAJOR.MINOR.PATCH` tag on main's history selects a
stable release. Both channels require successful platform, UI, security, speed,
and CodeQL checks for the exact source commit before publication, plus the
release workflow's native builds, live tests, and package installation checks.
Pages deployment runs independently and does not block package publication.
The publisher verifies the complete asset set, checksums, update metadata, and
GitHub upload digests before making the draft public. Rerunning a failed
Release workflow preserves its version and retries publication automatically.

Completed source workflows also trigger release recovery. If the CI gate timed
out before the final source check passed, recovery retries that failed
publication job using its existing package artifacts and version. It requires
all exact-source checks to pass, all package jobs to have succeeded, and the
only failed step to be the CI gate. It does not retry package or upload failures
or canceled runs, and it allows at most three attempts per release run.
The recovery job checks out the trusted workflow revision, executes no event
artifacts, and can request Actions retries without repository write permission.

After fixing an attestation error, dispatch the main `Release` workflow with
`recover_run` set to the failed run's numeric ID. This reuses packages only
when every native build and installer job succeeded, all source checks pass,
and the original failure was the CI gate or attestation step. It retains the
original version, source commit, and package build invocation. Publication
uses the current trusted workflow and records that revision separately in
provenance. Package, upload, canceled, fork, and branch runs are ineligible.

Linux releases also install both RPM variants on Amazon Linux 2023, configure
an ordinary user's engine access, run a real container, and remove the package.
The release matrix exercises native x86_64 and aarch64 binaries. A second check
boots Amazon Linux userspace under systemd in a disposable container and tests
the installed service, permissions, restarts, networking, Docker tools, and
removal. It verifies both immediate ACL access and the documented login
fallback, according to filesystem support. The data directory is checked after
the first container creates it.

These containers use private cgroup and network namespaces; they mount no host
cgroup tree or Docker socket and publish no host ports. The test uses Amazon
Linux userspace, the actual service unit, and the runner's kernel. To run the
systemd check locally:

```sh
bash packaging/linux/test-amazon-systemd.sh /path/to/native-packages /path/to/native-docker-cli
```

An optional third argument supplies a public RPM key and requires package
signature verification.

A successful release refreshes Pages automatically. The website publisher
verifies release provenance against the exact source and workflow commit,
requires that source in main's history, and adds RPM signatures using the
dedicated `ASAGIRI_RPM_SIGNING_KEY` secret. It publishes DNF metadata for both
architectures and generates the install page's download index. Publication
selects by version, so an older build finishing later cannot downgrade a
channel. The deploy check verifies the installer key pin, download URLs, and
RPM metadata over the live HTTPS site.

Mac packaging can use Developer ID signing, Apple notarization, stapled
tickets, and Gatekeeper assessment on both architectures. See
[macOS signing setup](macos-signing.md) for the five Actions secrets and the
required-signing policy. Without credentials it explicitly produces ad hoc,
unnotarized packages; it does not remove users' quarantine flags.

The separate [Homebrew tap](https://github.com/AndreKurait/homebrew-asagiri)
checks releases on a 15-minute schedule using its own `GITHUB_TOKEN`. It
verifies the source, provenance, and DMG checksums before creating a GitHub
signed and signed-off cask update. It needs no personal token or credential
that can write to both repositories.

To run the Linux build and unit tests locally with Docker:

```sh
sh scripts/ci/build-linux.sh \
  "$PWD/target/ci-bin" \
  "$PWD/target/ci-target" \
  "$PWD/target/ci-registry" tests
sh scripts/ci/live-tests.sh "$PWD/target/ci-bin"
sh scripts/ci/testcontainers.sh "$PWD/target/ci-bin" "$PWD/target/ci-maven"
```

The build guard requires at least 3 GiB free. The tests use privileged Linux
containers and publish no ports; they do not start the native macOS daemon.
`build` in place of `tests` builds the binaries without running unit tests.
Each output directory records the source commit in `SOURCE_COMMIT`.

To check the browser demo without regenerating artwork:

```sh
uv run --with-requirements scripts/ci/requirements-browser.txt \
  python scripts/render-demo.py --check
```

To regenerate the screenshots and animation, omit `--check` and also install
Pillow, as shown in the renderer's help text.
