#!/bin/bash
# Build the native GTK window and package/test its AppImage in a private host.
# Usage: appimage.sh NATIVE_BIN OUTPUT CACHE_DIR|volume:NAME
set -euo pipefail
if [ "$#" -ne 3 ]; then
  echo "usage: $0 NATIVE_BIN OUTPUT CACHE" >&2
  exit 2
fi
root=$(cd "$(dirname "$0")/../.." && pwd)
bin=$(cd "$1" && pwd)
test "$(cat "$bin/SOURCE_COMMIT")" = "$(git -C "$root" rev-parse HEAD)"
(cd "$bin" && sha256sum -c SHA256SUMS)
mkdir -p "$2"
out=$(cd "$2" && pwd)
cache=
case "$3" in
  volume:*)
    volume=${3#volume:}
    [[ "$volume" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]+$ ]] || exit 2
    [ "$(docker volume inspect --format '{{index .Labels "dev.asagiri.appimage-cache"}}' "$volume")" = 1 ]
    cache_mount=(--mount "type=volume,source=$volume,target=/cache")
    ;;
  *)
    mkdir -p "$3"
    cache=$(cd "$3" && pwd)
    cache_mount=(-v "$cache:/cache")
    ;;
esac
[ "$(df -Pk "$out" | awk 'END { print $4 }')" -ge 3145728 ] || { echo "Build guard: fewer than 3 GiB free" >&2; exit 1; }
image=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["image"])' "$root/packaging/linux/appimage-build.json")
case "$image" in rust:1.99.1-bookworm@sha256:*) ;; *) echo "Unexpected AppImage compiler image" >&2; exit 1 ;; esac
python3 - "$root" "$out/appimage-build-source.json" <<'PY'
import hashlib
import json
from pathlib import Path
import subprocess
import sys
root = Path(sys.argv[1])
def git(*args):
    return subprocess.check_output(["git", "-C", str(root), *args])
git("diff", "--exit-code", "HEAD", "--", ".", ":(exclude)Cargo.toml", ":(exclude)Cargo.lock")
assert not git("ls-files", "--unmerged")
files = {}
for name in git("ls-tree", "-r", "--name-only", "-z", "HEAD").decode().split("\0"):
    if name:
        path = root / name
        assert path.is_file() and not path.is_symlink(), name
        files[name] = hashlib.sha256(path.read_bytes()).hexdigest()
Path(sys.argv[2]).write_text(json.dumps({
    "source": git("rev-parse", "HEAD").decode().strip(),
    "source_tree": git("rev-parse", "HEAD^{tree}").decode().strip(),
    "files": files,
}, indent=2) + "\n")
PY
# AppImage's no-FUSE path executes extracted files from its temporary directory.
docker run --rm --tmpfs /tmp:exec,nosuid,nodev \
  -v "$root:/src:ro" -v "$bin:/bin-input:ro" -v "$out:/out" "${cache_mount[@]}" \
  -w /src -e CARGO_TARGET_DIR=/cache/target -e CARGO_HOME=/cache/cargo \
  -e CARGO_INCREMENTAL=0 -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
  "$image" bash -euo pipefail -c '
    test "$(df -Pk /cache | awk "END { print \$4 }")" -ge 3145728
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends libgtk-3-dev libwebkit2gtk-4.1-dev \
      python3 desktop-file-utils xvfb xauth xdotool
    test "$(rustc --version | cut -d" " -f2)" = 1.99.1
    # Reuse only dependencies; never let another source checkout supply the window.
    cargo clean --locked -p asagiri -p asagiri-image -p asagiri-proto -p asagiri-vmm -p asagiri-share-proto
    cargo clean --locked --release -p asagiri -p asagiri-image -p asagiri-proto -p asagiri-vmm -p asagiri-share-proto
    cargo test --locked -p asagiri --features window hostaccess::tests::
    cargo test --locked -p asagiri --features window ui::logins
    cargo build --locked --release -p asagiri --features window
    image=$(bash packaging/linux/appimage.sh /bin-input /cache/target/release/asagiri /out /cache/downloads)
    xvfb-run -a python3 tests/appimage.py "$image" /bin-input /cache/target/release/asagiri \
      --appimagetool "/cache/downloads/appimagetool-1.9.1-$(uname -m).AppImage" \
      --output /out/appimage-verification.json
    dpkg-query -W > /out/appimage-build-packages.txt
  '
if [ "$(uname -s)" = Linux ]; then
  sudo chown -R "$(id -u):$(id -g)" "$out"
  [ -z "$cache" ] || sudo chown -R "$(id -u):$(id -g)" "$cache"
fi
