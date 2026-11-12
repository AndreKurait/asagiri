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
