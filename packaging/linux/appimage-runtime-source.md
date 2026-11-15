# AppImage runtime source and notices

The AppImage runtime comes from AppImage/type2-runtime release `20251108`,
commit `dd6cebedcbddde9c82f89b011e8e1d40b6e43868`. The package builder checks its
architecture-specific SHA-256 before use. It also checks every source archive
listed in `appimage-sources.json`; those archives and their license notices are
included beside this file.

The runtime uses libfuse, squashfuse, musl, zstd, zlib and mimalloc. Its source
archive contains the build recipe, linker command and the libfuse patch.
The LGPL libfuse 3.15.0 source and that patch are included so the runtime can
be rebuilt with a modified library. The other archives provide the library
source and license material alongside the recipe's Alpine 3.21 environment;
this is not a claim that repeating the build produces identical bytes.

To rebuild, extract the type2-runtime source and follow its `BUILD.md`.
Its `scripts/common/install-dependencies.sh` specifies libfuse 3.15.0,
squashfuse 0.5.2 and the patch; the matching archives are in `sources/`.
The runtime is built by `scripts/build-runtime.sh`, using
`src/runtime/Makefile`. A replacement runtime can be supplied to
`appimagetool --runtime-file` when repacking the extracted AppDir.

`--appimage-extract` makes the AppDir, including these files, available without
FUSE. The GTK and WebKitGTK libraries used by Asagiri's window come from the
host system and are not bundled in this AppImage.
