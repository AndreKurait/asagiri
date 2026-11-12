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

See [RELEASES.md](../RELEASES.md) for release preparation. There is no Asagiri
Homebrew tap, package repository, hosted share service, or default update feed.
