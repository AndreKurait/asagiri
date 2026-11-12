# Continuous integration

Platform builds use Rust 1.99.1 on Linux, macOS, and Windows. Each platform
runs its tests and uploads development binaries with SHA-256 hashes. Linux
also runs the live Docker-compatible API tests inside a private test engine.
These artifacts are development builds, not signed product releases.

Dependency caches are tied to the compiler, platform, lockfile, manifests,
and Cargo configuration. Only pushes to `main` save caches; pull requests can
restore them. Workspace crates are rebuilt. The Linux helper removes their
artifacts before building and before saving the dependency cache, so a file
timestamp cannot cause the performance check to reuse the other revision's
engine.

The Linux and speed jobs also cache the Rust, Docker CLI, and Alpine base
images through GitHub Actions. `scripts/ci/images.json` pins both the registry
manifest and each architecture's image content ID. After loading an archive,
the helper verifies that content ID before exporting the image for a build.
A warm cache needs no registry request. A missing, damaged, or incorrect cache
falls back to a pull by digest and is replaced only after verification.
Release jobs additionally cache their Debian, Ubuntu, and Fedora images.
Test engines, their data directories, and their pulled test images stay fresh.

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

Primary references: [Actions cache](https://github.com/actions/cache),
[sccache's GitHub backend](https://github.com/mozilla/sccache/blob/main/docs/GHA.md),
[ECR pull-through cache](https://docs.aws.amazon.com/AmazonECR/latest/userguide/pull-through-cache.html),
and [Distribution mirrors](https://distribution.github.io/distribution/recipes/mirror/).

The speed check compares a PR's merge result with its base, or a main push
with the previous main commit. It builds both revisions with the same pinned
compiler. If both engine binaries match byte for byte, the check reports that
equality and skips measurements. Otherwise it runs the alternating comparison
with the baseline's limits. It does not silently substitute a retry for a
failed measurement.

The UI check runs the app's page tests and translation checks. The website
check also opens the assembled demo in Chrome and exercises navigation,
container actions, every settings tab, mirror edits, light/dark themes,
Arabic/English switching, and desktop/mobile layouts. Its check mode leaves
the committed images unchanged. Pages deploys only after those checks pass.
The deploy job then waits for `deployment.json` to name the source commit,
checks the public HTML and assets over HTTPS, and runs the interaction suite
again against the live project URL. Every push to main publishes automatically;
`workflow_dispatch` can retry a deployment without a source change.

Security checks include dependency advisories, licenses and sources,
ShellCheck, workflow auditing, and CodeQL. Advisory failures remain failures
on PRs as well as main. Superseded builds, UI tests, and CodeQL runs are
canceled so the newest revision gets the available runners.

To run the Linux build and unit tests locally with Docker:

```sh
sh scripts/ci/build-linux.sh \
  "$PWD/target/ci-bin" \
  "$PWD/target/ci-target" \
  "$PWD/target/ci-registry" tests
sh scripts/ci/live-tests.sh "$PWD/target/ci-bin"
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
