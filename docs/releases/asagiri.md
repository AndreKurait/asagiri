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
website, and CodeQL checks succeed, and all release packages build and pass
their available checks. Stable versions are published from `vMAJOR.MINOR.PATCH`
tags using the same workflow.

Linux package installation does not start the engine. Follow
`packaging/README.md` to configure host access. macOS VM startup and Windows
WSL2 first-run setup still require testing on real target systems. Do not
treat a CI package check as verification of those runtime paths.

Updates and sharing require explicit configuration. There is no automatic
migration of an existing Kiri installation. The guest kernel's corresponding
source is attached as `asagiri-kernel-source.tar.xz`.
