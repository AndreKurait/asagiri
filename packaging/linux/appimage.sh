#!/bin/bash
# appimage.sh <native-engine-directory> <GTK-window> <output-directory> [cache]
set -euo pipefail
umask 022
if [ "$#" -lt 3 ] || [ "$#" -gt 4 ]; then
  echo "usage: $0 <native-engine-directory> <GTK-window> <output-directory> [cache]" >&2
  exit 2
fi
[ "$(uname -s)" = Linux ] || { echo "Build the AppImage on Linux (a private build container also works)." >&2; exit 1; }
bindir=$1 window=$2 outdir=$3 cache=${4:-${XDG_CACHE_HOME:-$HOME/.cache}/asagiri-appimage}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1 | tr -d '\r')
for b in "$bindir/asagiri" "$bindir/asagiri-init" "$window"; do
  [ -x "$b" ] || { echo "no $b" >&2; exit 1; }
done
bindir=$(cd "$bindir" && pwd)
window=$(cd "$(dirname "$window")" && pwd)/$(basename "$window")
mkdir -p "$outdir" "$cache"
outdir=$(cd "$outdir" && pwd)
cache=$(cd "$cache" && pwd)
get() { # url file sum
  local f=$cache/$2 temporary
  if [ ! -f "$f" ]; then
    temporary=$(mktemp "$f.XXXXXXXX.part")
    if ! curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 -o "$temporary" "$1" \
      || ! echo "$3  $temporary" | sha256sum -c --quiet -; then
      rm -f -- "$temporary"
      echo "could not download and verify $1" >&2
      exit 1
    fi
    mv "$temporary" "$f"
  fi
  echo "$3  $f" | sha256sum -c --quiet - || { echo "$2 does not match the pinned SHA-256; delete $f and retry" >&2; exit 1; }
}
arch=$(uname -m)
case $arch in
  x86_64) tool_sum=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0 ;;
  aarch64) tool_sum=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158 ;;
  *) echo "no AppImage for $arch" >&2; exit 1 ;;
esac
python3 - "$arch" "$bindir/asagiri" "$bindir/asagiri-init" "$window" <<'PY'
from pathlib import Path
import struct
import sys
machine = {"x86_64": 62, "aarch64": 183}[sys.argv[1]]
for name in sys.argv[2:]:
    with Path(name).open("rb") as file:
        header = file.read(64)
    if len(header) != 64 or header[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", header, 18)[0] != machine:
        raise SystemExit(f"{name} must be a 64-bit Linux executable for {sys.argv[1]}")
PY
get "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$arch.AppImage" "appimagetool-1.9.1-$arch.AppImage" "$tool_sum"
python3 - <<'PY'
import os, stat
tmp = os.lstat("/tmp")
assert stat.S_ISDIR(tmp.st_mode) and tmp.st_uid == 0 and tmp.st_mode & stat.S_ISVTX
PY
work=$(mktemp -d /tmp/asagiri-appimage-build.XXXXXXXX)
trap 'rm -rf "$work"' EXIT
python3 "$root/scripts/ci/verify-linux-native.py" "$bindir" --arch "$arch" --output "$work/native-elf.json" >&2
app=$work/Asagiri.AppDir
install -D -m 755 "$bindir/asagiri" "$app/usr/bin/asagiri"
install -m 755 "$bindir/asagiri-init" "$app/usr/bin/asagiri-init"
install -m 755 "$window" "$app/usr/bin/asagiri-window"
install -m 755 "$here/AppRun" "$app/AppRun"
# The launcher runs the AppImage itself; the icon is the AppDir's asagiri.svg.
sed -e 's|^Exec=.*|Exec=asagiri|' -e 's|^Icon=.*|Icon=asagiri|' "$root/contrib/linux/asagiri.desktop" > "$app/asagiri.desktop"
install -m 644 "$root/brand/icon/asagiri-app-icon.svg" "$app/asagiri.svg"
mkdir -p "$app/usr/share/metainfo"
sed -e "s/@VERSION@/$version/" -e "s/@DATE@/$(date -u +%Y-%m-%d)/" "$root/contrib/linux/dev.asagiri.Asagiri.metainfo.xml" > "$app/usr/share/metainfo/dev.asagiri.Asagiri.metainfo.xml"
install -D -m 644 "$root/LICENSE" "$app/usr/share/doc/asagiri/LICENSE"
install -m 644 "$root/NOTICE" "$app/usr/share/doc/asagiri/NOTICE"
python3 "$here/appimage-sources.py" "$cache/sources" "$app/usr/share/doc/asagiri/appimage-runtime"
runtime=$work/runtime-$arch
bash "$here/appimage-runtime-build.sh" "$app/usr/share/doc/asagiri/appimage-runtime" "$runtime" >&2
install -m 644 "$runtime.json" "$app/usr/share/doc/asagiri/appimage-runtime/runtime-build.json"
tool=$work/appimagetool
install -m 755 "$cache/appimagetool-1.9.1-$arch.AppImage" "$tool"
# Verify the private copy, then isolate the tool's older extraction runtime.
echo "$tool_sum  $tool" | sha256sum -c --quiet -
install -d -m 700 "$work/tool-tmp"
out=$outdir/Asagiri-$version-$arch.AppImage
# Extract-and-run: build machines and containers often have no FUSE.
env -u TARGET_APPIMAGE TMPDIR="$work/tool-tmp" ARCH="$arch" APPIMAGE_EXTRACT_AND_RUN=1 \
  "$tool" --no-appstream --runtime-file "$runtime" "$app" "$out" >&2
echo "$out"
