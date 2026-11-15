#!/bin/bash
# Build the patched runtime from the source bundle, without package downloads.
# CI runs this inside the compiler image pinned in appimage-build.json.
set -euo pipefail
umask 022
[ "$(uname -s)" = Linux ] && [ "$#" -eq 2 ] || {
  echo "usage: appimage-runtime-build.sh SOURCE_BUNDLE OUTPUT_RUNTIME (Linux)" >&2
  exit 2
}
here=$(cd "$(dirname "$0")" && pwd)
bundle=$(cd "$1" && pwd)
output=$2
[ ! -e "$output" ] || { echo "Runtime output already exists" >&2; exit 1; }
for tool in gcc g++ make autoconf automake libtoolize pkg-config python3 patch ar strip objcopy readelf tar; do
  command -v "$tool" >/dev/null
done
python3 - "$here" "$bundle" <<'PY'
import hashlib, json, os, stat, sys
from pathlib import Path
here, bundle = map(Path, sys.argv[1:])
manifest = json.loads((here / "appimage-sources.json").read_text())
build = json.loads((here / "appimage-runtime-build.json").read_text())
for source in manifest["sources"] + build["tools"]:
    path = bundle / "sources" / source["name"]
    assert path.is_file() and not path.is_symlink()
    assert hashlib.sha256(path.read_bytes()).hexdigest() == source["sha256"], path
for name, expected in build["inputs"].items():
    path = here / name
    assert path.is_file() and not path.is_symlink()
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected, path
tmp = os.lstat("/tmp")
assert stat.S_ISDIR(tmp.st_mode) and tmp.st_uid == 0 and tmp.st_mode & stat.S_ISVTX
PY
work=$(mktemp -d /tmp/asagiri-runtime-build.XXXXXXXX)
cleanup_build() {
  local code=$?
  if [ "$code" -ne 0 ]; then
    for candidate in runtime libc-link-test; do
      if [ -f "$work/$candidate" ]; then
        cp -- "$work/$candidate" "$output.failed-$candidate"
        readelf -h -l -d "$work/$candidate" > "$output.failed-$candidate.txt" 2>&1 || true
      fi
    done
  fi
  rm -rf -- "$work"
  return "$code"
}
trap cleanup_build EXIT
mkdir "$work/src" "$work/install"
python3 - "$here" "$bundle" "$work/src" <<'PY'
import json, posixpath, subprocess, sys, tarfile
from pathlib import Path, PurePosixPath
here, bundle, destination = map(Path, sys.argv[1:])
sources = json.loads((here / "appimage-sources.json").read_text())["sources"]
tools = json.loads((here / "appimage-runtime-build.json").read_text())["tools"]
for source in sources + tools:
    path = bundle / "sources" / source["name"]
    with tarfile.open(path) as archive:
        for member in archive:
            name = PurePosixPath(member.name)
            assert not name.is_absolute() and ".." not in name.parts
            assert member.isdir() or member.isfile() or member.issym()
            if member.issym():
                target = PurePosixPath(posixpath.normpath(str(name.parent / member.linkname)))
                assert not target.is_absolute() and ".." not in target.parts
    # Python 3.11 in the pinned compiler image predates tarfile's data filter.
    # These exact archives are verified above; ignore their original ownership.
    subprocess.run(["tar", "--extract", "--file", str(path), "--directory", str(destination),
                    "--no-same-owner", "--no-same-permissions"], check=True)
PY
prefix=$work/install
jobs=${ASAGIRI_APPIMAGE_BUILD_JOBS:-2}
[[ "$jobs" =~ ^[1-9][0-9]?$ ]] || exit 2
unset CC CXX CFLAGS CXXFLAGS CPPFLAGS LDFLAGS PKG_CONFIG_PATH PKG_CONFIG_LIBDIR
# Ninja and Meson are tools, built/read from their separately pinned sources.
(
  cd "$work/src/ninja-2daa09ba270b0a43e1929d29b073348aa985dfaa"
  python3 configure.py --bootstrap
)
export PATH="$work/src/ninja-2daa09ba270b0a43e1929d29b073348aa985dfaa:$PATH"
meson=(python3 "$work/src/meson-1.5.2/meson.py")
# Build the linked libc too; host headers/libraries must not select glibc.
(
  cd "$work/src/musl-1.2.5"
  CFLAGS="-Os -fPIC" ./configure --prefix="$prefix" --disable-shared
  make -j"$jobs"
  make install
)
export CC="$prefix/bin/musl-gcc"
export CFLAGS="-Os -fPIC -ffunction-sections -fdata-sections"
export CPPFLAGS="-I$prefix/include"
export LDFLAGS="-static -L$prefix/lib"
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
# The pinned musl-gcc wrapper predates -static-pie support. Select musl's
# self-relocating startup object and suppress the interpreter for that mode.
python3 - "$prefix" <<'PY'
from pathlib import Path
import sys
prefix = Path(sys.argv[1])
path = prefix / "lib/musl-gcc.specs"
text = path.read_text()
old_start = "%{!shared: " + str(prefix / "lib/Scrt1.o") + "}"
new_start = "%{!shared: %{static-pie:" + str(prefix / "lib/rcrt1.o") + ";:" + str(prefix / "lib/Scrt1.o") + "}}"
assert text.count(old_start) == 1 and (prefix / "lib/rcrt1.o").is_file()
text = text.replace(old_start, new_start)
old_link = text.split("*link:\n", 1)[1].split("\n", 1)[0]
assert old_link.startswith("-dynamic-linker ") and old_link.count("-nostdlib") == 1
loader, remaining = old_link.removeprefix("-dynamic-linker ").split(" ", 1)
new_link = "%{!static-pie:-dynamic-linker " + loader + "} " + remaining
new_link = new_link.replace("-nostdlib", "-nostdlib %{static-pie:-static -pie --no-dynamic-linker -z text}")
path.write_text(text.replace(old_link, new_link))
PY
cat > "$work/libc-link-test.c" <<'C'
#include <stdio.h>
int main(void) { puts("static-pie libc link check"); return 0; }
C
"$CC" -fPIE -static-pie "$work/libc-link-test.c" -o "$work/libc-link-test"
python3 - "$work/libc-link-test" <<'PY'
import struct, subprocess, sys
from pathlib import Path
path = Path(sys.argv[1])
assert struct.unpack_from("<H", path.read_bytes(), 16)[0] == 3
assert b"INTERP" not in subprocess.check_output(["readelf", "-l", str(path)])
assert b"NEEDED" not in subprocess.check_output(["readelf", "-d", str(path)])
assert subprocess.check_output([str(path)]) == b"static-pie libc link check\n"
PY
(
  cd "$work/src/zlib-1.3.1"
  ./configure --static --prefix="$prefix"
  make -j"$jobs"
  make install
)
make -C "$work/src/zstd-1.5.6/lib" -j"$jobs" libzstd.a
install -m 644 "$work/src/zstd-1.5.6/lib/libzstd.a" "$prefix/lib/"
install -m 644 "$work/src/zstd-1.5.6/lib/zstd.h" "$work/src/zstd-1.5.6/lib/zstd_errors.h" "$prefix/include/"
runtime=$work/src/type2-runtime-dd6cebedcbddde9c82f89b011e8e1d40b6e43868
patch --batch --forward -d "$work/src/fuse-3.15.0" -p1 < "$runtime/patches/libfuse/mount.c.diff"
"${meson[@]}" setup "$work/fuse-build" "$work/src/fuse-3.15.0" \
  --prefix="$prefix" --libdir=lib --default-library=static \
  -Dutils=false -Dexamples=false -Dtests=false -Duseroot=false \
  -Ddisable-libc-symbol-version=true
"${meson[@]}" compile -C "$work/fuse-build" -j"$jobs"
"${meson[@]}" install -C "$work/fuse-build"
(
  cd "$work/src/squashfuse-0.5.2"
  ./autogen.sh
  ./configure --prefix="$prefix" --disable-shared --enable-static \
    --without-xz --without-lzo --without-lz4
  make -j"$jobs"
  make install
  mkdir -p "$prefix/include/squashfuse"
  install -m 644 ./*.h "$prefix/include/squashfuse/"
)
# Upstream's single-object static build preserves malloc override semantics.
"$CC" -Os -fPIC -ffunction-sections -fdata-sections -DMI_STATIC_LIB -DMI_MALLOC_OVERRIDE -DNDEBUG \
  -I"$work/src/mimalloc-2.1.7/include" -c "$work/src/mimalloc-2.1.7/src/static.c" \
  -o "$work/mimalloc.o"
ar rcs "$prefix/lib/libmimalloc.a" "$work/mimalloc.o"
patch --batch --forward -d "$runtime" -p1 < "$here/appimage-runtime.patch"
install -m 644 "$here/appimage-extraction.h" "$runtime/src/runtime/appimage-extraction.h"
python3 - "$here/appimage-runtime-build.json" "$runtime/src/runtime/runtime.c" <<'PY'
import hashlib, json, sys
from pathlib import Path
build = json.loads(Path(sys.argv[1]).read_text())
assert hashlib.sha256(Path(sys.argv[2]).read_bytes()).hexdigest() == build["patched_runtime_sha256"]
PY
version=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$here/appimage-runtime-build.json")
(
  cd "$runtime/src/runtime"
  "$CC" -std=gnu99 -Os -fPIE -D_FILE_OFFSET_BITS=64 "-DGIT_COMMIT=\"$version\"" \
    -I"$prefix/include" -I"$prefix/include/squashfuse" -I"$prefix/include/fuse3" \
    -T data_sections.ld -ffunction-sections -fdata-sections -Wl,--gc-sections \
    -static-pie -Wall -Werror runtime.c \
    -L"$prefix/lib" -lsquashfuse -lsquashfuse_ll -lzstd -lz -lfuse3 -lmimalloc \
    -o "$work/runtime"
)
strip --strip-debug --strip-unneeded "$work/runtime"
# These bytes must be added after stripping, as in upstream's build recipe.
python3 - "$work/runtime" <<'PY'
import struct, subprocess, sys
from pathlib import Path
path = Path(sys.argv[1])
elf = bytearray(path.read_bytes())
assert elf[:6] == b"\x7fELF\x02\x01"
assert struct.unpack_from("<H", elf, 16)[0] == 3
assert b"INTERP" not in subprocess.check_output(["readelf", "-l", str(path)])
assert b"NEEDED" not in subprocess.check_output(["readelf", "-d", str(path)])
elf[8:11] = b"AI\x02"
path.write_bytes(elf)
PY
install -m 755 "$work/runtime" "$output"
python3 - "$here" "$output" "$prefix" <<'PY'
import hashlib, json, platform, shutil, struct, subprocess, sys
from pathlib import Path
here, output, prefix = map(Path, sys.argv[1:])
build = json.loads((here / "appimage-runtime-build.json").read_text())
compiler = Path(shutil.which("gcc")).resolve()
elf = output.read_bytes()
offset = struct.unpack_from("<Q", elf, 40)[0]
size, count, strings_index = struct.unpack_from("<HHH", elf, 58)
sections = [struct.unpack_from("<IIQQQQIIQQ", elf, offset + i * size) for i in range(count)]
strings = elf[sections[strings_index][4]:sections[strings_index][4] + sections[strings_index][5]]
md5 = [section for section in sections if strings[section[0]:].split(b"\0", 1)[0] == b".digest_md5"]
assert len(md5) == 1 and md5[0][5] == 16 and elf[md5[0][4]:md5[0][4] + 16] == bytes(16)
toolchain = {}
for name in ("gcc", "g++", "ld", "ar", "strip", "objcopy", "make", "autoconf",
             "automake", "libtoolize", "pkg-config", "python3", "tar"):
    path = Path(shutil.which(name)).resolve()
    toolchain[name] = {
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "version": subprocess.check_output([name, "--version"], text=True).splitlines()[0],
    }
record = {
    "schema": "asagiri.appimage-runtime-build.v1",
    "status": "passed",
    "architecture": platform.machine(),
    "compiler_image": build["compiler_image"],
    "runtime_sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
    "runtime_version": build["version"],
    "runtime_source": json.loads((here / "appimage-sources.json").read_text())["runtime_commit"],
    "patched_runtime_sha256": build["patched_runtime_sha256"],
    "inputs": build["inputs"],
    "build_tools": {x["name"]: x["sha256"] for x in build["tools"]},
    "linked_source_archives": {
        x["name"]: x["sha256"]
        for x in json.loads((here / "appimage-sources.json").read_text())["sources"]
    },
    "packaging_md5_section": {"offset": md5[0][4], "size": 16, "initial_bytes": bytes(16).hex()},
    "toolchain": toolchain,
    "musl_gcc_sha256": hashlib.sha256((prefix / "bin/musl-gcc").read_bytes()).hexdigest(),
    "musl_gcc_specs_sha256": hashlib.sha256((prefix / "lib/musl-gcc.specs").read_bytes()).hexdigest(),
    "elf_type": "static-pie", "elf_interpreter": None, "elf_needed": [],
    "libc_static_pie_smoke": "passed",
    "linked_archives": {
        name: hashlib.sha256((prefix / "lib" / name).read_bytes()).hexdigest()
        for name in ("libc.a", "libz.a", "libzstd.a", "libfuse3.a",
                     "libsquashfuse.a", "libsquashfuse_ll.a", "libmimalloc.a")
    },
    "compiler_sha256": hashlib.sha256(compiler.read_bytes()).hexdigest(),
    "compiler_version": subprocess.check_output(["gcc", "--version"], text=True).splitlines()[0],
}
Path(str(output) + ".json").write_text(json.dumps(record, indent=2) + "\n")
PY
