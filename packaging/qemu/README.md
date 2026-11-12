# QEMU in Intel macOS builds

`cargo xtask qemu --arch x86_64` fetches the unmodified `qemu-aarch64-static`
asset from `multiarch/qemu-user-static` release `v7.2.0-1`, verifies the
SHA-256 pinned in `crates/asagiri/src/cli_pins.rs`, and stores it as
`guest/out/qemu/qemu-aarch64`. It runs as a separate program in the guest.

The emulator is QEMU 7.2.0, licensed under GPL version 2. `COPYING` is the
unmodified license from that QEMU tag. Intel app bundles include both files
beside the emulator.

- QEMU source: https://gitlab.com/qemu-project/qemu/-/tree/v7.2.0
- Source archive: https://download.qemu.org/qemu-7.2.0.tar.xz
- Distribution and build recipe: https://github.com/multiarch/qemu-user-static/tree/v7.2.0-1
- Binary release: https://github.com/multiarch/qemu-user-static/releases/tag/v7.2.0-1

There is no Asagiri binary release yet. When distributing a bundle containing
this emulator, provide its complete corresponding source, including the
build recipe and any statically linked dependencies, alongside the bundle.
