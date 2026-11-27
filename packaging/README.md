# Packaging Asagiri

These recipes also power the [automatic release workflow](../RELEASES.md).
The [release page](https://github.com/AndreKurait/asagiri/releases) carries
packages, installers, checksums, and provenance. The release notes state which
installation and runtime paths have been verified.

| Platform | Build | Package |
| --- | --- | --- |
| macOS | `cargo xtask app-dev` | local development app |
| macOS | `cargo xtask release`, then `cargo xtask dmg` | app, DMG, tarball and feed metadata |
| Windows | `cargo xtask windows-dev` | local development bundle |
| Windows | `cargo xtask msi` | per-user MSI |
| Linux | `cargo xtask linux` | static engine and guest agent |
| Linux | `scripts/ci/linux-packages.sh` | `.deb` and `.rpm` packages |
| Linux | `bash scripts/ci/build-linux-native.sh STATIC_BIN NATIVE_BIN CACHE tests` | GNU host engine and NSS checks |
| Linux | `bash scripts/ci/appimage.sh NATIVE_BIN OUTPUT CACHE` | native x86_64 or aarch64 AppImage |

Add `--arch x86_64` for Intel Macs or `--arch arm64` for Windows on Arm.
The guest and Linux packaging need Docker. WiX and the Windows SDK are required
for an MSI; macOS app packaging needs Xcode command line tools. The CLI tool
versions and download hashes are pinned in `crates/asagiri/src/cli_pins.rs`.

`cargo xtask release` creates the app, tarball, and feed; run `cargo xtask dmg`
to also create the disk image. Release CI builds the kernels on Linux and
passes their sparse archive to the macOS jobs. Windows CI receives tested
Linux binaries and the WSL rootfs through `ASAGIRI_WINDOWS_LINUX_DIR`, then
builds the complete installer without starting WSL on the packaging runner.

## Guest checkpoint runtime

Build the runtime for the guest architecture, then pass the resulting tar file to the initramfs builder. The Linux build container reads signed Debian snapshot indexes and stages CRIU 4.2.1, its helper programs, shared libraries and xtables plugins. The archive includes the binary and source package versions in `packages.json` and the package notices in `licenses/`. Keep it as a tar file on macOS so Linux names that differ only in case survive packaging.

```sh
python3 packaging/checkpoint/build.py --arch arm64 --output target/checkpoint/arm64.tar
ASAGIRI_CRIU_RUNTIME="$PWD/target/checkpoint/arm64.tar" cargo xtask guest --arch arm64
```

For the Intel guest, use `x86_64` in both commands. The build needs Docker with support for the selected Linux architecture. Choose a new archive path for another build.

Release CI builds each runtime on a Linux runner of the matching architecture. The macOS packaging job verifies that the app and update archive both contain the initramfs from that build. The `asagiri-checkpoint-source.tar` release asset includes both guest inventories, the runtime archive digests, the build recipes, and the complete Debian source packages. To build this asset locally, run the following command with the two runtime archives.

```sh
python3 packaging/checkpoint/source.py --arm64 target/checkpoint/arm64.tar \
  --x86-64 target/checkpoint/x86_64.tar --output target/asagiri-checkpoint-source.tar
```

## Fence the source during a handoff

On the destination, save `asagiri checkpoint platform` to `destination.json` and copy the file to the source. Use `asagiri checkpoint preflight` to review the dependencies before pausing. Capture and fence the retained containers together on the source. For two containers on an engine-managed bridge, run

```sh
asagiri checkpoint capture-group --destination destination.json --external-network migration web worker
```

With `--destination`, the command prints the compatibility and dependency report and captures the inspected container IDs. An incompatible guest architecture or missing required system mount target stops the operation before it pauses any member. The engine checks the members and checkpoint names before pausing. It pauses every running member before the first capture, keeps previously paused members paused, and holds their lifecycle locks through capture and fence persistence. If a member fails to capture, the engine discards this attempt’s completed captures and resumes the members it paused. It reports any failed cleanup by container. If fence persistence fails, keep the destination inactive and retry `asagiri checkpoint fence migration web worker` before handoff. Wait for the group command or fence retry to succeed before activating the destination.

For individual captures, pause all members first, capture each with the same name, then run `asagiri checkpoint fence migration web worker`. The engine checks each container’s most recent capture against its current pause. If a container ran again after capture, pause it and make fresh captures with a new name. A fenced source still supports checkpoint export. Docker unpause, stop, kill, restart and removal requests are refused. If the source engine shuts down, it terminates the frozen tasks without running their signal handlers, retains their checkpoints and prevents automatic or manual restart. The source engine’s container-inspect API exposes the fence name in `State.AsagiriCheckpointFence`.

To roll back while the original source processes are still paused, first confirm that the destination is inactive or has stopped. Run `asagiri checkpoint release migration web worker`, then `docker unpause web worker`. Release clears the fence and leaves the processes paused until you unpause them. Once the source engine has shut down, recovering the original process memory requires restoring the saved checkpoints. Release and an ordinary container start launch a fresh process in that case. Source fencing is a local operation. The operator coordinates destination activation and rollback across hosts.

### Restore into a destination bridge endpoint

For a container on an engine-managed bridge, `asagiri checkpoint create --external-network CONTAINER NAME` saves its processes and sockets with a reference to a destination network namespace. This lets the destination engine supply the endpoint, including its embedded DNS listener. The export records the CRIU resource key in `network.json` under `externalNetwork`. The endpoint needs the captured addresses, aliases and published ports. The destination also needs any custom interface, route or firewall configuration the workload depends on. `asagiri checkpoint create CONTAINER NAME` and `docker checkpoint create --leave-running CONTAINER NAME` capture the original network namespace.

On the destination, pass the exports for the whole group to `asagiri checkpoint prepare-networks` before creating endpoints. The command reads `network.json` from each archive and creates the missing user-defined bridges in one registry update. It checks existing definitions, including address pools and gateways, labels, driver options, IPv6, and internal and attachable settings. Other network drivers require a matching destination network. The result lists destination network IDs and whether this call created them. Repeating the command reuses those IDs. A conflicting definition leaves the registry as it was.

```sh
asagiri checkpoint prepare-networks backend.tar frontend.tar
```

`asagiri checkpoint create-receiver INPUT.tar CREATE.json` creates a stopped destination container with the captured name. Supply a Docker container-create request for the destination image, command, mounts and resource settings. The CLI prepares its bridges, applies the allocated IPv4 and IPv6 addresses, MAC addresses, DNS aliases, gateway priorities, resolver settings and actual published host ports. Existing network fields in the request must agree with the capture. Prepare linked containers first when the source used Docker links.

```sh
receiver=$(asagiri checkpoint create-receiver frontend.tar frontend-create.json)
asagiri checkpoint import "$receiver" received frontend.tar
asagiri checkpoint restore-rootfs "$receiver" received
asagiri checkpoint restore-network-files "$receiver" received
# For each captured directory volume, use its source name and destination name.
asagiri checkpoint restore-volume "$receiver" received "$source_volume" "$destination_volume"
```

`restore-rootfs` installs the captured writable layer over the receiver’s original image. It requires a freshly created container with an empty writable layer. Files copied into the receiver before restore cause a conflict. The command retains changes to image files, deletions, opaque directories, hard links, ownership and Linux extended attributes. Repeating it with the same checkpoint on the stopped receiver reuses the installed layer. You can use `docker cp` to inspect restored files before start.

`restore-network-files` installs the frozen `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` together on a freshly created receiver. Start binds these files from receiver storage. Edits remain visible to containers sharing its network namespace and survive restart. A receiver with custom mounts covering those files, or one sharing a donor’s network namespace, takes its network files from that configuration. Older archives can omit `network-files.tar`.

`restore-volume` restores one captured directory volume into a new local volume or one that has never been mounted. Use the name from `volume-mounts.json` as the source and the name used in the receiver’s create request as the destination. Restore one shared volume once and use that destination name for each receiver that shares it. The command publishes the complete tree together and accepts a retry with the same receiver, checkpoint and volume names. An existing data directory is preserved.

The engine claims the configured addresses and ports on start. A busy address or port can still prevent activation. An ordinary start runs the destination image’s command on the restored filesystem. Resuming the captured process still requires CRIU and binding its external mount references to the prepared receiver files and volumes.

Import stores the checkpoint. Manual restore opens the prepared endpoint’s network namespace and passes that descriptor to CRIU with `--inherit-fd fd[N]:asagiri-network`, where `N` is the open descriptor number. In two-container ARM64 tests between Mac and Debian engines on one physical Mac, the frontend kept its TCP connection to the backend in both directions. The external client reconnected to the original published port after source shutdown. On live rollback, the same external client socket remained usable.

For a non-TTY container, `stdio.json` records the CRIU pipe IDs for the input and output streams still connected to the source engine. `create-receiver` configures the terminal mode and Docker’s `StdinOnce` behavior, and opens the receiver’s input when the source still had an input pipe. A conflicting create request fails. Older archives keep the supplied terminal and input settings. A source pipe closed before capture retains CRIU’s ordinary closed-end behavior. The manifest records terminal mode for TTY containers. Reattaching a restored TTY remains unqualified.

To reconnect a non-TTY stream during manual process restore, pass an open descriptor for the corresponding receiver pipe with `--inherit-fd 'fd[N]:pipe:[ID]'`. Use the decimal `stdin`, `stdout` and `stderr` IDs in `stdio.json`. A `null` stream needs no mapping. For a mapped stdin, `stdin.bin` contains the unread bytes already in the source pipe followed by input waiting in the source agent. Once the receiver is running, call `asagiri checkpoint restore-stdin RECEIVER NAME` before attaching a new input client. The command registers the replay and returns while the guest feeds the saved bytes as the process reads. New attach input follows the saved bytes. With `close_after_input`, the engine closes its writer after the last saved byte. Retrying the same checkpoint on the same running receiver preserves its current replay position. Source clients should finish sending input before initiating a handoff.

For connected workloads, coordinate socket restore across the group. A Debian-to-Mac diagnostic restored both process trees into their destination namespaces, but its first request failed when the endpoints could exchange packets during restore. The passing procedure temporarily held traffic between the two peer addresses inside the destination namespaces. It ran CRIU from each receiver’s cgroup with `--manage-cgroups=ignore --leave-stopped`, checked that both restored PIDs belonged to the intended cgroups and network namespaces, removed the temporary peer rules, and continued both processes. The Mac engine provided the endpoints and published port. The CLI installed the captured filesystem, network files and named volumes before a diagnostic guest invoked CRIU.

To repeat the network checks, use disposable engines with Alpine 3.21 already loaded and the `10.218.47.0/24` subnet available. Compile the static workload for the Linux guest architecture. For ARM64 with the Rust musl target installed, run the following commands. `rollback` creates a new private output directory.

```sh
work=$(mktemp -d)
rustc --edition=2024 --target aarch64-unknown-linux-musl -C linker=rust-lld -O \
  tests/fixtures/checkpoint/stream_probe.rs -o "$work/stream-probe"
bash tests/checkpoint-stream.sh rollback "$work/rollback" "$work/stream-probe"
```

For a handoff, replace `rollback` with `capture`, give it a new output directory and pass the destination platform file as the fourth argument. The probe saves the dependency report in `preflight.json`. When it prints `FENCED`, stop the disposable source engine. The probe checks that its original external connection ends. Transfer the two archives privately and retain the `expected` directory for comparison. Restore both workloads into matching destination endpoints, then run `bash tests/checkpoint-stream.sh verify /path/to/expected` on the destination. The test opens the published port, checks both RAM-only identities and counters, compares the TCP endpoints, and resolves the backend name from the restored frontend.

After the network check, run `bash tests/checkpoint-stream.sh verify-stdio /path/to/expected BACKEND_ID FRONTEND_ID` with the destination container IDs. It checks separate Docker log streams for both original workloads, reads the byte left in the frontend’s input pipe before capture, then sends a new byte through Docker attach.

After rebooting the destination, run `bash tests/checkpoint-stream.sh verify-volumes BACKEND_VOLUME FRONTEND_VOLUME` with the names of the two destination volumes. The check mounts each existing volume read-only in a fresh container and compares `/data/stream` byte for byte with `ABCD`.

## Transfer a checkpoint privately

A checkpoint can contain credentials, process memory and volume contents. To transfer an export with GnuPG, use a source signing key and a destination encryption key. Exchange and verify their full fingerprints through a trusted channel first. The source needs the destination’s trusted public key. The destination needs the source’s public key and a GnuPG installation that supports `--assert-signer`.

On the source, set `CONTAINER`, `CHECKPOINT`, `SOURCE_FINGERPRINT` and `DESTINATION_FINGERPRINT`. Choose a new `TRANSFER_FILE` path outside the temporary directory.

```sh
(
  set -eu
  umask 077
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  asagiri checkpoint export "$CONTAINER" "$CHECKPOINT" "$work/checkpoint.tar" || exit 1
  gpg --no-encrypt-to --local-user "$SOURCE_FINGERPRINT" --recipient "$DESTINATION_FINGERPRINT" \
    --output "$TRANSFER_FILE" --sign --encrypt "$work/checkpoint.tar"
)
```

After this succeeds, copy the encrypted file to the destination. Set `TRANSFER_FILE` to its new path, `SOURCE_FINGERPRINT` to the expected signer, and `CONTAINER` and `CHECKPOINT` to the destination container and checkpoint name. Decrypt into a private directory. GnuPG can write plaintext before it finishes verifying an input, so run import only after successful verification.

```sh
(
  set -eu
  umask 077
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  gpg --batch --assert-signer "$SOURCE_FINGERPRINT" --output "$work/checkpoint.tar" --decrypt "$TRANSFER_FILE" || exit 1
  asagiri checkpoint import "$CONTAINER" "$CHECKPOINT" "$work/checkpoint.tar"
)
```

## Linux package assembly

On native Linux, live capture uses the host's `criu`, `iptables-restore`, `ip6tables-restore`, `tar` and `gzip`. Install these with the distribution package manager before starting the engine. The agent checks the kernel with `criu check` at startup. Run `asagiri checkpoint platform` to inspect `liveCapture` and, on Debian systems, the installed CRIU package version. A particular workload can still have devices or mounts CRIU cannot dump. Docker reports the CRIU error when a capture fails.

For engine-managed directory volumes, the CRIU post-dump hook runs GNU tar while the source process tree is stopped. The checkpoint contains `volume-mounts.json` and one `volumes/<name>/data.tar` per distinct volume. The manifest records `name`, `target` and `readOnly`. CRIU uses `asagiri-volume-<name>` as the external mount identifier. The archive preserves numeric ownership, permissions, links, sparse files, extended attributes and ACLs supported by GNU tar and the filesystem. Coordinate writes from outside the engine separately. `tests/checkpoint-volume.sh` exercises capture and export with two containers sharing a volume.

`asagiri checkpoint export` also includes a version 1 `network.json` for captures made with the current host and guest. It records the source network definitions, endpoint addresses and aliases, DNS and host overrides, exposed ports, requested bindings, and the host ports actually allocated. The engine records these while holding the container lifecycle lock. A later network change does not alter a saved checkpoint. The nested `network-files.tar` preserves ownership, permissions and contents of the engine-managed `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf` bind mounts, including container edits made before capture. These are the files needed by the corresponding CRIU external mount references. Older checkpoints can lack the network file archive or `network.json`. The destination CLI can prepare the network definitions as a group. The CLI also configures the receiver endpoints and can install the archived network files. Process restore still requires manual setup. Run `tests/checkpoint-network.sh` against an isolated engine with Alpine 3.21, `jq`, and `curl` to compare the export with Docker's inspection output.

To receive an exported checkpoint, create the destination container from the same image and run `asagiri checkpoint import CONTAINER NAME INPUT.tar` on the destination host. The engine checks the captured base layer digests when a writable layer is present and publishes the name after the upload finishes. An interrupted upload can be retried with the same name. Use `asagiri checkpoint ls CONTAINER` to see the saved checkpoint. Import stores the process images, volume archives and metadata. Use the restore commands above to install the filesystem data. Process resume still requires the diagnostic CRIU procedure.

```sh
packaging/linux/fetch-cli.sh target/linux-cli
sh scripts/ci/build-linux.sh target/portable-bin target/portable-cache target/cargo-registry build
bash scripts/ci/build-linux-native.sh target/portable-bin target/native-bin target/native-cache tests
bash packaging/linux/build.sh target/native-bin target/packages x86_64 target/linux-cli
```

`build.sh` needs `dpkg-deb` and `rpmbuild`. The full package includes the Docker
CLI, Compose and buildx; `asagiri-lite` includes only the engine. Service names,
group, executable paths, socket, and data directories use Asagiri's identity.
Installing the package does not start the engine. Set up host access explicitly
with `sudo asagiri host-access "$USER"`. Membership grants host-level access.

Native packages link the headless engine against the host's GNU C library so
account and group lookups use its configured NSS providers. The ELF gate limits
the engine to glibc 2.34 and standard GNU runtime libraries; packages declare
glibc and libgcc dependencies. The pinned Debian builder runs a real NSS module
with account lookup, actual Unix peer credentials, socket ACL admission,
provider outages and live membership revocation. It restores the disposable
builder's NSS configuration and verifies its account files remain unchanged.
Real SSSD or Himmelblau service deployments still need their own integration
coverage.

Platform CI also measures both engine builds against pinned Docker and Podman
on the same Linux runner: API ping, container listing, a complete container
run, exec, and a checked 64 KiB copy round trip. It preserves 400 samples for
each workload and engine in each build comparison. These workloads do not
establish cold image-pull, full application or directory-service performance.

The init executable, portable Linux archives and Windows/WSL inputs remain
static. Portable musl builds do not load host GNU NSS modules. The package
assembly and release scripts keep the two engine builds separate and check
their source and binary digests.

## Linux AppImage

The AppImage bundles the GNU engine, static guest agent and GTK window. It uses the
host's GTK 3 and WebKitGTK 4.1 libraries; the window is built on Debian 12 and
requires glibc 2.36 or later. Docker's CLI, Compose and Buildx remain separate
host tools. A compatible systemd service is required to run the engine.

Make the downloaded file executable, then launch it:

```sh
chmod +x Asagiri-VERSION-x86_64.AppImage
./Asagiri-VERSION-x86_64.AppImage
```

On a host without FUSE, use `APPIMAGE_EXTRACT_AND_RUN=1` with the same command.
Passing CLI arguments, such as `--version`, runs the headless engine instead of
opening the window. The onboarding password prompt can install the engine and
agent in `/usr/local/bin` and create an Asagiri systemd service. Existing
services and different installed executables are preserved. Host access grants
the account control of the engine and its containers.

If **Start engine** reports `couldn't look up group asagiri` after you
authenticate, check the `group:` providers in `/etc/nsswitch.conf`. On a
Zorin OS 18.1 live desktop, `sss` was listed even though its NSS socket was
absent. An unavailable provider can make a missing group indistinguishable
from an existing directory group. On a directory-managed host, restore the
provider and retry. On a standalone host where the administrator has confirmed
that `asagiri` should be a new local group, run `sudo groupadd --system asagiri`
and verify that `getent group asagiri` returns the new group. If it does not,
repair the group lookup configuration so it reaches the local group database.
Once the group resolves, select **Start engine** again. Setup handles membership
and service installation.

The AppImage does not update a system-installed engine automatically. Continue
using the package manager for an existing package installation. For an engine
installed through AppImage onboarding, stop the service and explicitly replace
both installed executables together when updating.

`appimage.sh` builds and checks a native AppImage from source using the pinned
Rust/Debian container. `NATIVE_BIN` must contain this commit's GNU engine,
static agent, `SHA256SUMS` and `SOURCE_COMMIT`, as produced by
`scripts/ci/build-linux-native.sh`.
CI checks the extracted executables, CLI version, desktop metadata, linked
window libraries, runtime sources and a visible GTK window under a private X
server. That launch check uses software rendering and disables WebKit's sandbox
only inside its disposable test process; it does not exercise a real desktop's
password prompt or FUSE mounting.

The AppImage tool and runtime downloads are pinned by SHA-256. Each AppImage
contains the runtime's source archives, LGPL relinking/build instructions and
license notices under `usr/share/doc/asagiri/appimage-runtime`. See
[runtime source](linux/appimage-runtime-source.md) for the exact components.

## Installer identity and artwork

The Windows MSI has an Asagiri-specific UpgradeCode and component GUIDs. The
updater checks that UpgradeCode as well as version and architecture. macOS
bundle identifiers, Linux desktop metadata, WSL distribution names, and data
paths are distinct from Kiri. There is no automatic data migration.

```sh
python3 brand/generate.py brand
python3 brand/render.py brand
python3 scripts/demo.py
```

The renderer needs librsvg (`rsvg-convert`) and Pillow. It creates PNG and ICO
icons, a monochrome menu-bar template, and 24-bit WiX bitmaps. Windows stamps the
version onto the committed artwork during packaging.

See [RELEASES.md](../RELEASES.md) for release preparation and
[installation](../docs/install.md) for Homebrew and the signed Amazon Linux
2023 repository. The setup script detects the architecture and verifies its
download. Website publication refreshes the package repository after releases.
There is no hosted share service or default in-app update feed.
