Asagiri is an independent Docker-compatible engine derived from Kiri.

The 0.1.7 changes include:

- Image inspection preserves stored `Cmd` and `Entrypoint` values, including
  `null` and explicitly empty arrays. Missing fields fall back to the image's
  execution configuration.

The 0.1.6 changes include:

- Image archive loading finishes filesystem layer preparation before reporting
  success. Extraction failures are returned during load, and completed layers
  are reused by later container creation and concurrent loads.
- Failed container commands report their startup error before returning
  success. Missing and non-executable commands return HTTP 400 with exit
  codes 127 and 126; inspect retains the error across daemon restarts and
  clears it after a successful retry. Post-start exec and `--init` exits
  keep their process-exit behavior.
- PATH lookup preserves permission failures and still tries later entries.
  Cleanup errors retain the command-rejection type, and serialized state
  snapshots prevent an older failure from overwriting a successful retry.
  Non-regular passwd and group files no longer block account lookups.
- Bounded cgroup shutdown with pinned process handles, nested and threaded
  group handling, and cleanup that recognizes an already removed group
  without targeting a replacement at the same path.
- Custom seccomp argument corrections and compact compilation of adjacent
  equality rules. Dense profile tests retain holes, action changes and
  out-of-range values. Profiles share the complete 1 MiB request budget;
  oversized requests fail before writing bytes or consuming a reply.
- Container roots remain ready after successful copies and process exit,
  preserving quick starts and restarts. Removal, reconciliation and shutdown
  still wait for owned cleanup and reclaim the retained overlays.
- Docker-compatible DNS selection when an alias appears on multiple
  attached networks, preserving routing and gateway-family precedence
  before network-name order.
- Creation of a container's working directory before the user switch and
  read-only root setup. Existing directory metadata is preserved; an exec
  with a missing working directory fails without creating it.
- Native Linux packages that resolve directory accounts through GNU NSS,
  with separate portable musl engine artifacts.
- Preservation of implicit volume labels and driver options, anonymous
  volume lifetime, and existing volume metadata.
- Armenian language support, bringing the interface and setup to 39
  languages, plus source-linked benchmark results in the README.

The ordinary Docker/Podman/Asagiri comparison retains 6,000 samples across
five API workloads. An optional baseline/candidate/candidate/baseline run
retains 24,000 samples on one host, including p95 and slow lifecycle counts.
These measurements cover the specified small API operations; they do not
establish general application throughput. Verus verification remains scoped
to the production IPAM gap-search function described below.

On macOS, unavailable shell profiles no longer interrupt the app's first
setup. Existing profile contents are preserved when they cannot be read.
Fatal setup errors identify the failing operation, and opening the app
from Finder or the Dock presents an error alert if setup or the window fails.
The log identifies these launches as `app-launch`.

Dense IPv4 and IPv6 networks find free addresses by merging occupied and
excluded hosts in order. A Verus proof checks the production gap search's
bounds, lowest-free result, exhaustion and overflow behavior. CI also compares
the actual allocation methods across 26 paired workloads, records an
unchanged-source control with identical executables, and retains the raw
timings. These checks supplement the existing engine performance gates.
Automatic pool exhaustion returns Docker's HTTP 500 response on start and
network connect. Live checks cover dense dual-stack allocation, static and
auxiliary reservations, range boundaries, failed allocation and retry.

Containers inherit image labels when created, and explicit container labels
override matching image labels. Inspect and label-filtered discovery retain
the merged labels across daemon restart. Invalid label values return HTTP
400 before mount side effects.

If container removal reaches the filesystem cleanup deadline, the stopped
container remains available with its writable data intact. A later removal
can retry, and the destroy event is published only after cleanup succeeds.
The cleanup deadline and lifecycle lock remain in place.

Explicit removal also runs outside the agent's control loop. A slow unmount
or directory deletion leaves unrelated exec requests and DNS responsive.
Queued and active removals reserve their container IDs until the worker exits;
failed forks and abnormal worker exits retry with a delay. One worker runs at
a time, so a stuck removal can still delay later removals and shutdown.

Quiet container waits, log followers and output-only attached sessions release
their API connection when the client disconnects. Clients that only close
their write half still receive the response. The startup
`ASAGIRI_MAX_CONNECTIONS` setting accepts 1 to 4096 concurrent connections,
with a default of 256; operators must budget threads, descriptors and memory
and leave capacity for control requests.

Containers started with `--init` release inherited sockets, pipes and namespace
handles before supervision. Stdio, signal forwarding and child reaping remain
available, including for TTY and nonroot containers.

Image imports preserve FIFOs, character and block device nodes, their
ownership, permissions, modification times and links. Local PAX time records
retain nanosecond precision; invalid size, ownership or time overrides fail before
replacing a node. File, gzip stdin, chunked API, HTTP URL and save/load routes
are covered by comparisons with Docker.

Replacing an image tag retains the previous image ID, configuration and layers
for existing containers. An image without another tag remains available as a
dangling image, and retagging it removes the redundant dangling record.
Existing aliases retain their registry provenance. Regression checks cover
concurrent replacements, shared layers, saved images and container commits.

Images pulled by registry digest keep that reference in `RepoDigests`.
List, inspect, disk usage and saved archives exclude digest references and
image IDs from `RepoTags`, so Docker can load digest-only exports. Saving
several references to one image retains its real tags once and preserves
the configuration and layer contents.

Forced image deletion by ID removes all references to that image in one
index update. Removing a named tag or digest preserves other references
and shared layers. Running containers prevent image-ID deletion even with
force; forcing the last tag off a running image keeps it dangling. A stopped
container's image can be forcibly deleted while retaining the data needed
to restart that container, until its last container is removed.

Rejected container starts release their embedded DNS listener, names and
parent rootfs mount before returning the setup error. This prevents repeated
failed starts from accumulating sockets and mounts while preserving other
containers' DNS registrations. Writable data follows the existing removal
policy: an automatically removed container may discard its data.
Failed attempts also retire their virtual network links before a later attach
reuses the address, even if the old network namespace remains alive. The live
regression also deletes and recreates networks on the same subnet.

Unprivileged containers fail before application startup if Linux cannot load
or attach the cgroup device-access filter. Unless automatic removal is enabled,
their identity and writable data remain available for retry after correcting
the host restriction. Existing image and bind-mounted device nodes also need access filtering, so dropping
the ability to create new nodes is no longer used as a fallback. Both bundled
guest kernels enable the required BPF/cgroup support.

Linux CI runs a pinned DDEV PHP/MariaDB project, including real SQL-backed
HTTP and trusted HTTPS requests through Traefik before and after a restart.
The test uses a private engine and checks the downloaded CLI, image digests,
derived base layers and cleanup.

Linux firewall guidance explains bridge egress under a host `FORWARD DROP`
policy. A disposable-host regression checks scoped forwarding allowances,
NAT, unsolicited inbound blocking and coexistence with a real Docker daemon.
The host's global policy and each engine's firewall ownership are preserved.

The engine now serves Docker's built-in BuildKit driver. Builds can load
images directly, receive contexts and secrets, reuse cache, export files,
resolve `host-gateway`, and cancel without blocking the next build. Setup
preserves existing builder choices and cache while enabling the native driver
for `asagiri build`. The build-history view includes the managed backend once.

Private registry settings now persist across restarts and apply to image
pulls, pushes, and managed BuildKit builds. `asagiri config` shows and updates
registry mirrors, explicit insecure-registry exceptions, and concurrent
download/upload limits. TLS verification remains enabled by default;
exceptions apply only to the configured authorities or literal IP ranges.
Real HTTP, authenticated HTTP, self-signed HTTPS, policy revocation, and
parallel transfer checks run against private registry fixtures.

Private HTTPS registries can also use `registry-ca-certificates` to trust an
explicit certificate authority while retaining chain, hostname and expiry
verification. The engine and managed BuildKit receive the same certificate
bytes, and a changed policy replaces the engine's connection pools. Trust
changes are rejected before persistence while builds are active. A failed
BuildKit configuration update remains pending until installation and startup
succeed. Invalid or oversized bundles are rejected before changing the builder
or saved settings. See [registry CA certificates](../registry-ca-certificates.md) for
the authority scope, accepted PEM format and client boundaries.

Stopped-container archive requests now finish after their worker releases
its mounts. Linux service recovery also reclaims stale and stacked mounts
when the old cgroups have already been removed, preserving container data.
CI exercises delayed archive completion, repeated service restarts, crash
recovery, and a real kind cluster with node and pod DNS checks. The exact
CachyOS/systemd environment from the upstream reports remains unverified.

Container-name filters accept regular expressions while preserving exact
matches. Invalid patterns are ignored without affecting valid filters.

GitHub Actions measures container lifecycle latency, API latency, memory,
system calls, and binary size for each commit against its baseline. The
[performance dashboard](https://andrekurait.github.io/asagiri/performance/)
keeps the raw samples and commit history. Separate scheduled checks compare
Asagiri, Docker, and Podman on the same Linux host; those measurements cover
short container lifecycles, not VM startup or application throughput.

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

The UI now includes 39 languages, a searchable language picker, and custom
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

Successful archive uploads now consume the complete framed body and keep the
connection available for the next request. Pooled Docker clients can start
the builder on that connection. Error responses, image import/load and classic
build responses still advertise closure when needed. Thirty-three framing
checks cover the upload-to-start sequence, gzip, chunk trailers, padding,
large uploads and damaged archives.

Image lists return labels and support label filters, including images indexed
before labels were cached. Container creation reuses one parsed image config
for volumes, ports and stop signals. The performance workflow adds metadata
creation and archive-upload measurements, with 15 metrics and all 4,000 timing
samples retained per comparison.

Overlay mounts disable metadata-only copy-up and directory redirects so
committed layers contain complete files after chmod or rename. Older incomplete
upper entries return an explicit error. Exclusive-upper protection and the
existing copy-worker and daemon mount cleanup remain in force. Private Linux
tests verify committed file hashes, overlap rejection and restart recovery.

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
  The AppImage for x86_64 or aarch64 includes the GTK window, static engine and
  agent. It requires the host's GTK 3, WebKitGTK 4.1 and glibc 2.36 or later;
  Docker CLI tools remain separate. The window's explicit host setup preserves
  existing services and installed executables. Use `APPIMAGE_EXTRACT_AND_RUN=1`
  where FUSE is unavailable. CI checks extraction, CLI execution, bundled
  runtime source and a headless GTK launch; real desktop authorization and FUSE
  mounting remain unverified.
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
