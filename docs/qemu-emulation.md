# ARM64 containers in the Intel guest

Intel Mac packages include a static x86_64 `qemu-aarch64` built from QEMU
11.0.5 source. The guest uses it for ARM64 Linux images. Apple Silicon runs
ARM64 natively and does not need this emulator.

The [build recipe](../packaging/qemu/README.md) pins QEMU, Buildroot and the
configuration for an internal musl toolchain. The build directly runs an
ARM64 probe covering threads, atomics, memory mappings, processes, pipes and
files. Bundle checks validate ELF architecture, absence of a dynamic loader
or shared-library dependencies, checksums, recipe identity and licenses.
They inspect Mac app contents without starting a VM.

On an Intel Mac running macOS 26.6.1, the published `0.1.14-dev.158` app
pulled `linux/arm64` Alpine 3.21. Inside the container, `uname -m` reported
`aarch64`, and a shell pipeline wrote and read a file. Running the same tag
with `--platform linux/amd64` then reported `x86_64`.
[Native Intel Mac verification](https://github.com/AndreKurait/asagiri/actions/runs/44943623664).

## Verified source build

The full build and ARM64 execution probe passed in [GitHub Actions run 43633676310](https://github.com/AndreKurait/asagiri/actions/runs/43633676310) from source `4b4aeab085c7fe17b21c4801825784676879dd09`. The downloaded artifact matched GitHub's ZIP digest. Independent inspection verified the static emulator, both archives, all 236 files and hardlinks in the source manifest, the source pins, configuration, six recipe files, and 44 target and host package entries. No code from the archive was executed during that inspection.

The verified corresponding-source archive has SHA-256 `da2db0b713aa3a0febc517a47486e39def472f55b4c877662cdf2c9cc84af16d`. This identifies that build; future recipe changes have their own build records.

## Releases and source

Every release with the emulator also includes `asagiri-qemu-source.tar.xz`.
It contains the exact emulator and dependency sources, target and host
license manifests, patches, internal compiler, musl, configuration and build
scripts. The app carries its emulator build record, QEMU's `COPYING` and the
target package licenses. The emulator hash in the source archive must match
the app's build record.

`Intel ARM emulation` and `Release` use the same build action. Only successful
main pushes save finished products in the shared cache. Restored products
must match the source pins, recipe, artifact hashes and extracted bundle
checks before use. PR caches cannot replace main's cache entries. A release
cannot publish without the source archive or a successful emulator job.

## Source builds

Build on x86_64 Linux using the prerequisites and command in the recipe.
Transfer all three output files to the Mac: `build.json`,
`asagiri-qemu-guest.tar.gz` and `asagiri-qemu-source.tar.xz`.

Then import them into a checkout of the same recipe:

```sh
ASAGIRI_QEMU_PRODUCTS=/absolute/path/to/qemu-products \
  cargo xtask qemu --arch x86_64
cargo xtask app --arch x86_64
```

The default product directory is `target/qemu-products`. Import verifies
both archives and installs the emulator plus licenses in `guest/out/qemu`.
It reuses an existing matching bundle and rejects a different one; use a
fresh generated output directory when upgrading the recipe. `xtask app`
verifies the bundle again before copying it into `Resources/qemu`.

No build or validation step changes the host's binfmt registrations.
