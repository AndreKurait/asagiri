# Asagiri releases

The [release page](https://github.com/AndreKurait/asagiri/releases) lists Asagiri
builds. `.github/workflows/release.yml` publishes them automatically.
Historical Kiri notes in `docs/releases/` remain attributed to Kiri; the shared
Asagiri release notes are in `docs/releases/asagiri.md`.

## Automatic prereleases

Every main build whose Platform builds, UI tests, Security, Speed, Website,
and CodeQL workflows succeed can publish one immutable prerelease. All gates
must refer to the exact source commit. Forks, PR runs, other branches, failed
checks, and missing checks cannot publish.

The version is `RELEASE_VERSION-dev.N`, where `N` is the Platform builds run
number, for example `0.1.0-dev.42`. Repeated workflow events for the same build
reuse that tag and skip an already published release. A workflow completion
reruns the gate, so the last successful check starts packaging without a
manual step. `workflow_dispatch` on main can retry an interrupted release.

The pipeline builds Linux packages and static engines on native x86_64 and
arm64 runners; macOS apps and DMGs on Apple Silicon and Intel; and Windows
x86_64 and arm64 installers and ZIP bundles. It runs release-version unit
tests, Linux live tests, Linux package installation/removal, app signature and
DMG verification, and x86_64 MSI installation/removal without starting WSL.
Windows Arm packages are cross-built. Native VM and WSL runtime checks remain
separate; see the platform limits in the release notes.

Publication is the last step: require the complete asset set, validate feed
versions and hashes, write `SHA256SUMS` and `release-manifest.json`, attest the
artifacts, upload to a draft, compare GitHub's asset digests with the local
files, and only then publish. A failure leaves the release unpublished.
Published releases and assets are never overwritten. Prereleases do not
replace the stable release selected by GitHub's `latest` link.
The SLSA provenance records both the package source commit and the workflow
commit, including when main advances while an earlier checked build is packaging.

## Stable releases

Update the notes and, when preparing the next release series, `RELEASE_VERSION`
on main. After its checks pass, create a signed version tag in a checkout of
this repository:

```sh
git tag -s v0.1.0 -m 'Asagiri 0.1.0'
git push origin v0.1.0
```

Tags must be `vMAJOR.MINOR.PATCH` and point to a commit in main's history.
The same checks, package builds, and upload verification apply. The workflow
sets the version in Cargo.toml and the workspace's lockfile entries only in
its checkout; source checkouts continue to report `0.0.0`.

macOS apps are ad hoc signed, without Apple notarization; Windows installers
are not Authenticode signed. Checksums and GitHub build provenance accompany
both channels. The optional Intel QEMU emulator is omitted until its complete
corresponding source and static dependency sources are packaged for distribution.

Regenerate branding and demos when they change, and third-party notices when
dependencies change. The guest kernel's complete corresponding source is an
asset of every release.

`ASAGIRI_RELEASE_BASE` can select a release artifact location. The package
builder defaults to this repository's versioned GitHub release path.
`ASAGIRI_UPDATE_URL` must be set explicitly to use an update feed. Updater
checks preserve the app/MSI identity and target architecture.
