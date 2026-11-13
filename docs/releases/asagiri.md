Asagiri is an independent Docker-compatible engine derived from Kiri.

This release includes the Asagiri identity and artwork, the container app,
Docker import, registry mirrors, and the reviewed upstream changes recorded
in `UPSTREAM.md`.

The full upstream PR audit also brings automatic guest recovery for running
`always` and `unless-stopped` containers, with bounded backoff and manual-stop
cancellation. Old VM resources and exit accounting finish before replacement.
Registry-backed Buildx tags resolve the requested platform while locally
created tags remain available. Private Linux tests exercise real agent crashes,
restart counts, retained files, and local and cross-platform Buildx builds.

Buildx configuration uploads now advertise closed HTTP connections correctly,
so pooled Docker clients reconnect before starting the builder. The same fix
covers image import/load and classic build responses. Regression checks cover
the upload-to-start sequence and its connection headers.

Containers and execs now have independent process sessions, so process-group
signals cannot stop the engine. The UI's engine card follows the actual engine
state after every Start/Stop control. HTTP clients requesting connection closure
receive EOF, and empty command/entrypoint arrays follow Docker's image defaults.
Windows usage accounting excludes unrelated processes with recycled parent PIDs.
The MSI installation check normalizes native Windows paths before invocation.

Archive uploads preserve numeric owners by default. With `copyUIDGID=true`,
they use the configured container user, including named users and primary
groups; an empty configured user keeps the archive owners. Regression tests
compare Docker and Asagiri across running and stopped containers, gzip
uploads, file modes, and links.
Uploads also follow Docker's file/directory replacement rules and
`noOverwriteDirNonDir` error status, while preserving symlink targets.
Directory timestamps are restored after child extraction, including nested
directories, merged uploads, repeated headers, and Unix epoch timestamps.

Independent layer uploads can unpack concurrently while duplicate layers
remain serialized. Truncated uploads and corrupt gzip tails cannot leave an
installed layer. The [performance report](https://github.com/AndreKurait/asagiri/blob/main/docs/layer-transfer-performance.md)
records the measured Linux workload and its limits. Engine inspection also
reports the host name, preserves its daemon ID across restarts, returns
uncompressed RootFS layer digests, and omits absent healthchecks.

The optional Linux GTK window uses a backport of the upstream GLib iterator
safety fix (GHSA-wrw7-89jp-8q8g). Optimized iterator regressions run in CI;
the source, license, and patch provenance are included in `vendor/glib`.

Choose the download for your system:

- **Linux:** `.deb` or `.rpm` for amd64/x86_64 or arm64/aarch64. `asagiri`
  bundles the Docker CLI, Compose, and buildx; `asagiri-lite` contains the
  engine. These packages currently provide the command line interface.
  Static engine tarballs are also included.
- **macOS:** the DMG for Apple Silicon (`arm64`) or Intel (`x86_64`).
  Apps are ad hoc signed and are **not notarized by Apple**. Intel packages
  run native x86_64 containers; the optional QEMU emulator is not bundled.
- **Windows:** the MSI for x86_64 or arm64. Windows packages are **not
  Authenticode signed**. A ZIP of the same bundle is also available.
  WSL2 is required to run containers. CI tests MSI installation and removal
  on both x86_64 and Arm Windows runners without starting WSL. The arm64
  MSI is cross-built and then checked on Windows Arm.

Versions containing `-dev.` are automatic prereleases for testing. They
are published only after the source commit's platform, UI, security, speed,
and CodeQL checks succeed, and all release packages build and pass
their available checks. Stable versions are published from `vMAJOR.MINOR.PATCH`
tags using the same workflow.

For direct downloads, Homebrew, or the signed Amazon Linux 2023 repository,
use the [installation guide](https://andrekurait.github.io/asagiri/install/).
The Amazon Linux setup command installs the package and configures host access;
installing a raw RPM alone does not start the engine. macOS VM startup and Windows
WSL2 first-run setup still require testing on real target systems. Do not
treat a CI package check as verification of those runtime paths.

Updates and sharing require explicit configuration. There is no automatic
migration of an existing Kiri installation. The guest kernel's corresponding
source is attached as `asagiri-kernel-source.tar.xz`.
