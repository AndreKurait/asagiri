# AppImage runtime source and notices

The AppImage runtime is built from AppImage/type2-runtime release `20251108`,
commit `dd6cebedcbddde9c82f89b011e8e1d40b6e43868`, with Asagiri's included
`appimage-runtime.patch` and `appimage-extraction.h`. The patch replaces the
shared extraction cache with private directories for each invocation, validates
AppRun before execution, and retains a directory until its own child exits.
Termination during extraction is deferred until cleanup, before AppRun starts.
Elevated extraction ignores `TMPDIR` and requires a root-owned sticky `/tmp`.
Normal no-FUSE execution still needs an executable temporary filesystem.

The builder checks every library archive in `appimage-sources.json` and every
tool archive and local input in `appimage-runtime-build.json`. All are included
here, with their license notices. The linked libfuse, squashfuse, musl, zstd,
zlib and mimalloc are built from these sources. Meson and Ninja are build tools;
their pinned sources are also included. CI uses the compiler container identified
by digest in `appimage-runtime-build.json`; `runtime-build.json` records the
actual compiler and runtime hashes. This does not claim byte-identical rebuilds.

To rebuild on Linux, run `bash appimage-runtime-build.sh . /path/to/runtime`
from this directory, with GCC/G++, GNU make/binutils, autoconf, automake,
libtool, pkg-config and Python 3.11 available. The script does not download or
install packages. Its upstream libfuse 3.15.0 patch is inside the runtime source
archive, and the LGPL library source is in `sources/`. Change the source archive
and its manifest hash when rebuilding with a modified library. A replacement
runtime can be supplied to `appimagetool --runtime-file` when repacking the
extracted AppDir.

`--appimage-extract` makes the AppDir, including these files, available without
FUSE. The GTK and WebKitGTK libraries used by Asagiri's window come from the
host system and are not bundled in this AppImage.
