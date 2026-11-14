# ARM64 emulation in the Intel guest

The Intel guest can run ARM64 Linux programs through a separate, static
`qemu-aarch64` executable. QEMU 11.0.5 is built from its official source with
Buildroot 2026.08.1 and an internal musl toolchain. Only the ARM64 Linux user
target is enabled. The emulator is GPL-2.0; the accompanying `COPYING` comes
from the exact QEMU source that was built.

`sources.json` pins both source archives. Their SHA-256 values were checked
against the upstream release signatures:

- Buildroot: `18C7DF2819C1733D822D599EA500D6EE9CB0E540`.
- QEMU: `CEACC9E15534EBABB82D3FA03353C9CEF108B584`.

Buildroot's QEMU recipe is updated from 11.0.0 to the stable 11.0.5 patch
release. Its existing patches and dependency versions are retained. Buildroot
checks every downloaded source against its hash and validates the available
license hashes; QEMU's own downloads are disabled during compilation.

On x86_64 Ubuntu 24.04:

```sh
sudo apt-get install build-essential bison flex gettext libncurses-dev rsync \
  unzip file bc cpio wget libssl-dev python3 xz-utils \
  gcc-aarch64-linux-gnu libc6-dev-arm64-cross
python3 packaging/qemu/build.py build --work /tmp/asagiri-qemu-build \
  --output /tmp/asagiri-qemu
```

Use a fresh work directory with enough space for an internal compiler build.
The build stops at checkpoints if fewer than 3 GiB remain. It checks that
the result is a static x86_64 ELF and directly runs an ARM64 program covering
arguments, threads, atomics, memory mappings, processes, pipes and files.
This test does not register binfmt handlers or boot a macOS VM.

The output includes the emulator bundle, a build record and
`asagiri-qemu-source.tar.xz`. The source archive contains:

- The exact Buildroot archive, build configuration and this build recipe.
- All downloaded sources, including the internal compiler and musl.
- Buildroot's target and host source/license manifests, applied patches,
  license texts and source checksums.
- The emulator hash, dependency versions and recipe hashes.

Source collection fails on missing archives, licenses or unresolved
Buildroot warnings. The one expected warning says Buildroot's own source was
not collected; that archive is explicitly included and verified separately.

To rebuild from the extracted corresponding source without relying on the
upstream archives remaining available:

```sh
sha256sum --check SHA256SUMS
python3 recipe/build.py build --source-bundle "$PWD" \
  --work /tmp/asagiri-qemu-rebuild --output /tmp/asagiri-qemu-rebuilt
```

The source bundle supplies the download cache; standard host build tools
are still required. Buildroot and QEMU are separate programs; bundling them
does not change Asagiri's Apache-2.0 license.

The `Intel ARM emulation` workflow validates this source-built bundle.
Native Intel macOS VM execution still needs hardware validation.
