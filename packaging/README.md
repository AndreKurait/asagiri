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
| Linux | `bash scripts/ci/appimage.sh STATIC_BIN OUTPUT CACHE` | native x86_64 or aarch64 AppImage |

Add `--arch x86_64` for Intel Macs or `--arch arm64` for Windows on Arm.
The guest and Linux packaging need Docker. WiX and the Windows SDK are required
for an MSI; macOS app packaging needs Xcode command line tools. The CLI tool
versions and download hashes are pinned in `crates/asagiri/src/cli_pins.rs`.

`cargo xtask release` creates the app, tarball, and feed; run `cargo xtask dmg`
to also create the disk image. Release CI builds the kernels on Linux and
passes their sparse archive to the macOS jobs. Windows CI receives tested
Linux binaries and the WSL rootfs through `ASAGIRI_WINDOWS_LINUX_DIR`, then
builds the complete installer without starting WSL on the packaging runner.

## Linux package assembly

```sh
packaging/linux/fetch-cli.sh target/linux-cli
packaging/linux/build.sh target/linux-x86_64-musl target/packages x86_64 target/linux-cli
```

`build.sh` needs `dpkg-deb` and `rpmbuild`. The full package includes the Docker
CLI, Compose and buildx; `asagiri-lite` includes only the engine. Service names,
group, executable paths, socket, and data directories use Asagiri's identity.
Installing the package does not start the engine. Set up host access explicitly
with `sudo asagiri host-access "$USER"`. Membership grants host-level access.

## Linux AppImage

The AppImage bundles the static engine, guest agent and GTK window. It uses the
host's GTK 3 and WebKitGTK 4.1 libraries; the window is built on Debian 12 and
requires glibc 2.36 or later. Docker's CLI, Compose and Buildx remain separate
host tools. A compatible systemd service is required to run the engine.

Make the downloaded file executable, then launch it:

```sh
chmod +x Asagiri-VERSION-x86_64.AppImage
./Asagiri-VERSION-x86_64.AppImage
```

On a host without FUSE, use `APPIMAGE_EXTRACT_AND_RUN=1` with the same command.
Passing CLI arguments, such as `--version`, runs the static engine instead of
opening the window. The onboarding password prompt can install the engine and
agent in `/usr/local/bin` and create an Asagiri systemd service. Existing
services and different installed executables are preserved. Host access grants
the account control of the engine and its containers.

The AppImage does not update a system-installed engine automatically. Continue
using the package manager for an existing package installation. For an engine
installed through AppImage onboarding, stop the service and explicitly replace
both installed executables together when updating, or install a current package.

`appimage.sh` builds and checks a native AppImage from source using the pinned
Rust/Debian container. `STATIC_BIN` must contain this commit's static engine,
agent and `SOURCE_COMMIT` file, as produced by `scripts/ci/build-linux.sh`.
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
