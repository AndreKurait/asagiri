#!/bin/bash
# Build the GNU host engine separately from static portable and guest binaries.
# Tests install a real NSS provider only inside this disposable compiler host.
set -euo pipefail
if [ "$#" -ne 4 ]; then
  echo "usage: build-linux-native.sh STATIC_BIN OUTPUT CACHE tests|build" >&2
  exit 2
fi
root=$(cd "$(dirname "$0")/../.." && pwd)
static=$(cd "$1" && pwd)
mkdir -p "$2" "$3"
output=$(cd "$2" && pwd)
cache=$(cd "$3" && pwd)
mode=$4
case "$mode" in tests|build) ;; *) exit 2 ;; esac
test "$(df -Pk "$output" | awk 'END { print $4 }')" -ge 3145728
test "$(cat "$static/SOURCE_COMMIT")" = "$(git -C "$root" rev-parse HEAD)"
(cd "$static" && sha256sum -c SHA256SUMS)
image=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["image"])' "$root/packaging/linux/native-build.json")
case "$image" in rust:1.99.1-bookworm@sha256:*) ;; *) echo "Unexpected native compiler image" >&2; exit 1 ;; esac
cp "$static/SOURCE_COMMIT" "$output/SOURCE_COMMIT"
python3 - "$root" "$output/native-build-source.json" <<'PY'
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
assert not git("ls-files", "--others", "--exclude-standard"), "Commit the source inputs before qualification"
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
# The quoted script runs wholly inside the compiler container.
# shellcheck disable=SC2016
docker run --rm --tmpfs /tmp:exec,nosuid,nodev \
  -v "$root:/src:ro" -v "$static:/portable:ro" -v "$output:/out" -v "$cache:/cache" \
  -w /src -e CARGO_TARGET_DIR=/cache/target -e CARGO_HOME=/cache/cargo \
  -e CARGO_INCREMENTAL=0 -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
  -e ASAGIRI_NSS_PRIVATE_BUILDER=1 -e GITHUB_RUN_ID -e GITHUB_RUN_ATTEMPT \
  "$image" bash -euo pipefail -c '
    test "$(df -Pk /cache | awk "END { print \$4 }")" -ge 3145728
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends python3 binutils
    test "$(rustc --version | cut -d" " -f2)" = 1.99.1
    clean_source() {
      cargo clean --locked -p asagiri -p asagiri-image -p asagiri-proto -p asagiri-vmm -p asagiri-share-proto
      cargo clean --locked --release -p asagiri -p asagiri-image -p asagiri-proto -p asagiri-vmm -p asagiri-share-proto
    }
    clean_source
    cargo build --locked --release -p asagiri
    install -m 755 /cache/target/release/asagiri /out/asagiri
    install -m 755 /portable/asagiri-init /out/asagiri-init
    python3 scripts/ci/verify-linux-native.py /out --arch "$(uname -m)" --output /out/native-elf.json
    if [ "$1" = tests ]; then
      cargo test --locked -p asagiri --no-run --message-format=json > /out/native-tests-build.jsonl
      binary=$(python3 -c "import json; rows=[json.loads(x) for x in open(\"/out/native-tests-build.jsonl\")]; matches=[x[\"executable\"] for x in rows if x.get(\"reason\")==\"compiler-artifact\" and x.get(\"profile\",{}).get(\"test\") and x.get(\"executable\") and x[\"target\"][\"name\"]==\"asagiri\"]; assert len(matches)==1, matches; print(matches[0])")
      python3 tests/nss-directory.py --binary /out/asagiri --unit-test-binary "$binary" \
        --static-control /portable/asagiri --out /out/nss
    fi
    (cd /out && sha256sum asagiri asagiri-init > SHA256SUMS)
    rustc -vV > /out/rustc.txt
    dpkg-query -W > /out/compiler-packages.txt
    clean_source
  ' asagiri-native "$mode"
test "$(sha256sum "$static/asagiri-init" | cut -d' ' -f1)" = "$(sha256sum "$output/asagiri-init" | cut -d' ' -f1)"
if [ "$(uname -s)" = Linux ]; then
  sudo chown -R "$(id -u):$(id -g)" "$output" "$cache"
fi
