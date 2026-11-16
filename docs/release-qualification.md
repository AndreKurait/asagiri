# Release qualification

Every main push runs the Engine comparison workflow. Release publication waits for a successful comparison at that exact commit, alongside the platform, UI, security, CodeQL and speed checks. The existing release retry workflow resumes publication when the comparison finishes.

The comparison measures ping, an empty filtered list, container lifecycle, exec output and exit status, and a 64 KiB file round trip through a local volume. Asagiri, Docker and Podman receive the same pinned Alpine image and network settings. Each workload retains all 400 samples per engine, validates its result, and verifies removal of its own fixtures. The [comparison notes and retained results](engine-comparison.md) describe host limitations; these API workloads do not establish application throughput or native desktop performance.

Mac preview identity is checked separately. The published `v0.1.6-dev.98` executable correctly reported `0.1.6-dev.98`, while both numeric Apple bundle fields reported `0.1.6`. The updater previously used that shortened field for its release comparison. Bundles now add `AsagiriVersion` with the complete release version. The updater prefers this explicit identity and continues to read older stable bundles; a malformed explicit identity is rejected.

Both Mac release jobs require the expected bundle identity, native architecture, actual executable `--version`, and matching plist and executable bytes in the update archive. They retain a separate package-identity artifact without changing the public asset manifest. These checks run after the existing code-signature verification.

The metadata tests exercise xtask's actual plist template and reject the old preview metadata, a stale executable, the wrong architecture, changed archive contents and duplicate archive identity entries. Native Rust tests cover preview ordering, stable compatibility and malformed properties. The installed official dev.98 bundle served as a negative control for the new metadata gate. Its app bytes were left unchanged.
