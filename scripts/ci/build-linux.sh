#!/bin/sh
# Run from the source tree to build. Dependency caches may be shared between
# revisions; workspace crates are always rebuilt, so a cache cannot select
# the wrong engine for the live tests or the performance comparison.
set -eu
if [ "$#" -ne 4 ]; then
  echo "usage: build-linux.sh BIN_DIR TARGET_DIR REGISTRY_DIR tests|build" >&2
  exit 2
fi
output=$1
target=$2
registry=$3
mode=$4
case "$mode" in tests|build) ;; *) exit 2 ;; esac
mkdir -p "$output" "$target" "$registry"
output=$(cd "$output" && pwd -P)
fixtures="${output}-fixtures"
target=$(cd "$target" && pwd -P)
registry=$(cd "$registry" && pwd -P)
free_kib=$(df -Pk "$target" | awk 'END { print $4 }')
if [ "$free_kib" -lt 3145728 ]; then
  echo "Build guard: fewer than 3 GiB free" >&2
  exit 1
fi
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
set -- docker run --rm --tmpfs /tmp
if [ "$mode" = tests ]; then
  mkdir -p "$fixtures"
  set -- "$@" --privileged -v "$fixtures:/fixtures"
fi
"$@" -v "$PWD:/src:ro" -v "$output:/out" -v "$target:/t" \
  -v "$registry:/usr/local/cargo/registry" -w /src -e CARGO_TARGET_DIR=/t \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=cc \
  -e CARGO_INCREMENTAL -e CARGO_PROFILE_DEV_DEBUG -e CARGO_PROFILE_TEST_DEBUG \
  "${ASAGIRI_CI_RUST_IMAGE:-rust:1.99.1-alpine}" sh -ec '
    apk add --no-cache -q build-base
    set -- "$1" crates/*-init/Cargo.toml
    [ "$#" -eq 2 ] && [ -f "$2" ]
    mode=$1
    engine=${2#crates/}; engine=${engine%/Cargo.toml}; engine=${engine%-init}
    clean_workspace() {
      set -- -p "$engine" -p "$engine-init" -p "$engine-image" \
        -p "$engine-proto" -p "$engine-vmm"
      # Older comparison baselines predate the shared protocol crate.
      if [ -f "crates/$engine-share-proto/Cargo.toml" ]; then
        set -- "$@" -p "$engine-share-proto"
      fi
      cargo clean --locked "$@"
      cargo clean --locked --release "$@"
    }
    clean_workspace
    if [ "$mode" = tests ]; then
      export ASAGIRI_REQUIRE_MOUNTED_FILE_VIEW_TEST=1
      cargo test --locked -p "$engine" -p "$engine-init" -p "$engine-proto" -p "$engine-image" -p "$engine-vmm"
      rustc --edition=2024 -C opt-level=1 -C panic=abort tests/wsl-daemon-fixture.rs -o /fixtures/asagiri-daemon-fixture
      (cd /fixtures && sha256sum asagiri-daemon-fixture > SHA256SUMS)
    fi
    cargo build --release --locked -p "$engine" -p "$engine-init"
    cp "/t/release/$engine" "/t/release/$engine-init" /out/
    clean_workspace
    cd /out
    sha256sum "$engine" "$engine-init" > SHA256SUMS
  ' asagiri-ci "$mode"
git rev-parse HEAD > "$output/SOURCE_COMMIT"
if [ "$mode" = tests ]; then git rev-parse HEAD > "$fixtures/SOURCE_COMMIT"; fi
# The cache action runs as the runner user, while the compiler runs as root.
if [ "$(uname -s)" = Linux ]; then
  sudo chown -R "$(id -u):$(id -g)" "$target" "$registry"
fi
