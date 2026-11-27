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

## Linux package assembly

On native Linux, live capture uses the host's `criu`, `iptables-restore`, `ip6tables-restore`, `tar` and `gzip`. Install these with the distribution package manager before starting the engine. The agent checks the kernel with `criu check` at startup. Run `asagiri checkpoint platform` to inspect `liveCapture` and, on Debian systems, the installed CRIU package version. A particular workload can still have devices or mounts CRIU cannot dump. Docker reports the CRIU error when a capture fails.

For engine-managed directory volumes, the CRIU post-dump hook runs GNU tar while the source process tree is stopped. The checkpoint contains `volume-mounts.json` and one `volumes/<name>/data.tar` per distinct volume. The manifest records `name`, `target` and `readOnly`. CRIU uses `asagiri-volume-<name>` as the external mount identifier. The archive preserves numeric ownership, permissions, links, sparse files, extended attributes and ACLs supported by GNU tar and the filesystem. Coordinate writes from outside the engine separately. `tests/checkpoint-volume.sh` exercises capture and export with two containers sharing a volume.

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
