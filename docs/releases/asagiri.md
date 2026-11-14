Asagiri is an independent Docker-compatible engine derived from Kiri.

The engine now serves Docker's built-in BuildKit driver. Builds can load
images directly, receive contexts and secrets, reuse cache, export files,
resolve `host-gateway`, and cancel without blocking the next build. Setup
preserves existing builder choices and cache while enabling the native driver
for `asagiri build`. The build-history view includes the managed backend once.

An optional local browser interface shows real containers, images, volumes,
networks and recent logs. Set a password with `asagiri web password`, enable
access with `asagiri web enable`, then run `asagiri web`. It listens on
loopback and checks the password, host and origin on requests. A separate
permission allows start, stop and restart. Disabling access or rotating the
password revokes existing access.

AI clients can use a local stdio MCP server after access is enabled in
Settings → AI integrations or with `asagiri mcp enable`. Seven read tools
return bounded metadata and recent logs. Starting, stopping and restarting
existing containers requires a separate permission. Connected clients honor
revocation, cancellation and request deadlines. Mac, Windows and Linux share
the permission controls and connection configuration.

This release includes the Asagiri identity and artwork, the container app,
Docker import, registry mirrors, and the reviewed upstream changes recorded
in `UPSTREAM.md`.

The UI now includes 38 languages, a searchable language picker, and custom
container address pools with overlap checks. USB devices can reconnect after
returning to the bus; stopping the engine cancels pending reconnects and
container restarts. The guest kernel includes the macOS 15 Rosetta fix.
Screenshots and the interactive demo reflect these additions.

Concurrent image-index changes are serialized, and a complete index with stale
trailing bytes is backed up before recovery. Docker SDKs receive a compatible
engine version while `AsagiriVersion` reports the actual product version.
Engine information reports available memory capacity before the first
container starts, without waking a sleeping guest.
Live checks cover log-based readiness, container root setup before immediate
execs, and macvlan/ipvlan link names. VM disk trimming now runs in its own
process so the agent remains single-threaded when cloning containers.

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

Container exits are reported before slow filesystem cleanup finishes.
Restart, removal, and archive operations for that container wait for cleanup;
unrelated execs and DNS stay responsive. Cleanup records retain container
identity even after Linux reuses the old process ID. Stopped-container copies
now honor persistent bind and volume mounts, including read-only mounts.
Requested port bindings remain visible through create, start, and stop.
Image loading waits for the complete HTTP body before publishing its contents.
Cleanup workers are killed if their agent exits, before replacement resources
can reuse their paths.

The Mac memory balloon now reserves active and mapped file cache and waits
30 seconds after pressure or substantial cache refaults before shrinking.
Policy and protocol regressions cover the new accounting and gradual release;
native Mac cache retention and throughput have not been measured.

The embedded resolver leaves wildcard port 53 free for container DNS services.
Default-bridge BuildKit DNS uses a reachable gateway for WSL/local host
resolvers. Buildx's default parent cgroup now groups builder containers under
Asagiri's delegated tree with aggregate accounting and limits. Unsupported
custom parent cgroups, user remapping, and bridge interface
names return clear errors. Windows publishing explains the WSL/Windows LAN
boundary; it does not install Windows forwarding or firewall rules.

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
  Default builds are ad hoc signed and are **not notarized by Apple**.
  The release workflow supports Developer ID signing and notarization when
  the required Apple credentials are configured. Intel packages run native
  x86_64 containers and include QEMU 11.0.5 for ARM64 Linux images, with its
  licenses and complete source archive. Linux emulation probes and bundle
  inspection run in CI; native Intel Mac VM execution remains unverified.
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
