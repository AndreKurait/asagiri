#!/bin/bash
# Package the GNU host engine and keep static portable/WSL inputs separate.
set -euo pipefail
if [ "$#" -ne 4 ]; then
  echo "usage: release-linux.sh PORTABLE_BIN OUTPUT WSL_INPUTS NATIVE_BIN" >&2
  exit 2
fi
bin=$(cd "$1" && pwd)
out=$2
inputs=$3
native=$(cd "$4" && pwd)
test "$(cat "$bin/SOURCE_COMMIT")" = "$(git rev-parse HEAD)"
test "$(cat "$native/SOURCE_COMMIT")" = "$(cat "$bin/SOURCE_COMMIT")"
(cd "$bin" && sha256sum -c SHA256SUMS)
(cd "$native" && sha256sum -c SHA256SUMS)
arch=$(uname -m)
case "$arch" in x86_64|aarch64) ;; *) exit 2 ;; esac
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])')
mkdir -p "$out" "$inputs"
out=$(cd "$out" && pwd)
inputs=$(cd "$inputs" && pwd)
cli=$PWD/target/linux-cli-$arch
bash packaging/linux/fetch-cli.sh "$cli" "$HOME/.cache/asagiri-linux-cli" "$arch"
bash packaging/linux/build.sh "$native" "$out" "$arch" "$cli"
ASAGIRI_EXPECTED_VERSION="$version" bash packaging/linux/test.sh "$out" docker "$cli"
ASAGIRI_EXPECTED_VERSION="$version" bash packaging/linux/test-amazon.sh "$out" "$cli"
ASAGIRI_EXPECTED_VERSION="$version" bash packaging/linux/test-amazon-systemd.sh "$out" "$cli"
cp "$bin/asagiri" "$bin/asagiri-init" "$inputs/"
sh scripts/asagiri-linux.sh "$PWD" "$bin" "$inputs/asagiri-linux.tar.gz" "$arch"
python3 - "$inputs" "$version" "$arch" <<'PY'
import hashlib
import json
from pathlib import Path
import subprocess
import sys
folder = Path(sys.argv[1])
files = ("asagiri", "asagiri-init", "asagiri-linux.tar.gz")
metadata = {
    "version": sys.argv[2],
    "arch": sys.argv[3],
    "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "sha256": {name: hashlib.sha256((folder / name).read_bytes()).hexdigest() for name in files},
}
(folder / "build.json").write_text(json.dumps(metadata, indent=2) + "\n")
PY
tar -czf "$out/asagiri-$version-linux-$arch.tar.gz" \
  -C "$bin" asagiri asagiri-init -C "$PWD" LICENSE NOTICE
