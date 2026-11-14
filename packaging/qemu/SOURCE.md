# Emulator source and licenses

This directory contains QEMU 11.0.5, a separate GPL-2.0 program by Fabrice
Bellard and the QEMU contributors. `COPYING` is copied from the source that
produced this executable. `manifest.csv` lists Buildroot's target packages;
their license texts are in `licenses/`. Some packages provide build inputs
and are not linked into the emulator.

Every Asagiri release that includes this executable also distributes
`asagiri-qemu-source.tar.xz` alongside the installers:

https://github.com/AndreKurait/asagiri/releases

Choose the same Asagiri release as this app. The archive contains the exact
QEMU, Buildroot, musl, compiler and dependency sources, patches, license texts,
configuration and build scripts. Its `build.json` records the same emulator
SHA-256 as this directory's `build.json`. Instructions in `README.md` rebuild
the emulator from those sources. The release's checksums and GitHub build
provenance cover the source archive.

The emulator is shared into the Intel Linux guest as a separate executable.
Asagiri's Rust workspace remains Apache-2.0.
