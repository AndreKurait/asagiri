#!/bin/sh
# Asagiri's eight Linux packages from this checkout, on any Debian or Ubuntu
# with Rust, both GNU cross compilers, python3-venv, rpm and dpkg-deb: WSL's
# Ubuntu on a Windows PC, or the container linux-packages.sh starts on a Mac.
# - the GNU host engine and static guest init for x86-64 and arm64;
#   scripts/windows-engine.sh supplies the separate static guest binaries
# - the docker CLI, Compose and buildx at their pins
#   (packaging/linux/fetch-cli.sh), kept in target/linux-cli-<arch> for the
#   host test (packaging/linux/test-host.sh)
# - the window (asagiri --features window, against this machine's glibc, GTK
#   and WebKitGTK) for this machine's own architecture, when
#   libwebkit2gtk-4.1-dev is installed; it goes in the full asagiri package
# - asagiri and asagiri-lite as .deb and .rpm for each (packaging/linux/build.sh)
# - an AppImage for the native architecture when the GTK window is available;
#   its source-built runtime also needs GCC/G++, make/binutils, autoconf,
#   automake, libtool, pkg-config and Python 3.11 (CI pins their compiler image)
#
#   scripts/ci/linux-build.sh
#
# Leaves them and SHA256SUMS-linux in target/packages. zig, the crates and
# the build stay in ~/.cache between runs.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
rm -rf target/packages
mkdir -p target/packages
# The other architecture's .rpm is assembled here too, from cross-compiled
# binaries; rpmbuild has to be told it may package that architecture.
grep -qs "^buildarch_compat: $(uname -m):" "$HOME/.rpmrc" || echo "buildarch_compat: $(uname -m): x86_64 aarch64 noarch" >> "$HOME/.rpmrc"
bins=$(mktemp -d)
trap 'rm -rf "$bins"' EXIT
# Its own target dir: the same crate with another feature set would otherwise
# rebuild over the engine's build each time. No --locked, as for the engine:
# a test build sets Cargo.toml to <release>-dev.N, which changes the lock file.
window=
if pkg-config --exists webkit2gtk-4.1 2>/dev/null; then
  # shellcheck source=/dev/null
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  # In ~/.cache, as the engine's is (scripts/windows-engine.sh): under WSL a
  # checkout on C: is slow for cargo's many small files.
  wtarget=$HOME/.cache/asagiri-window-target/$(printf %s "$PWD" | sha256sum | cut -c1-12)
  CARGO_TARGET_DIR=$wtarget cargo build --release -p asagiri --features window
  window=$wtarget/release/asagiri
else
  echo "no libwebkit2gtk-4.1-dev: the packages have no window" >&2
fi
for arch in x86_64 aarch64; do
  bash scripts/windows-engine.sh "$root" "$bins/portable-$arch" "$arch"
  compiler=$arch-linux-gnu-gcc
  command -v "$compiler" >/dev/null || { echo "Install $compiler for the native package build" >&2; exit 1; }
  target=$arch-unknown-linux-gnu
  rustup target add "$target"
  ntarget=$HOME/.cache/asagiri-native-target/$(printf %s "$PWD" | sha256sum | cut -c1-12)
  cc_key=CC_$(printf %s "$target" | tr '-' '_')
  linker_key=CARGO_TARGET_$(printf %s "$target" | tr '[:lower:]-' '[:upper:]_')_LINKER
  env "$cc_key=$compiler" "$linker_key=$compiler" CARGO_TARGET_DIR="$ntarget" \
    cargo build --locked --release --target "$target" -p asagiri
  mkdir -p "$bins/$arch"
  install -m 755 "$ntarget/$target/release/asagiri" "$bins/$arch/asagiri"
  install -m 755 "$bins/portable-$arch/asagiri-init" "$bins/$arch/asagiri-init"
  git rev-parse HEAD > "$bins/$arch/SOURCE_COMMIT"
  (cd "$bins/$arch" && sha256sum asagiri asagiri-init > SHA256SUMS)
  bash packaging/linux/fetch-cli.sh "target/linux-cli-$arch" "$HOME/.cache/asagiri-linux-cli" "$arch"
  w=; [ "$arch" = "$(uname -m)" ] && w=$window
  set -- "$bins/$arch" target/packages "$arch" "target/linux-cli-$arch"
  [ -z "$w" ] || set -- "$@" "$w"
  bash packaging/linux/build.sh "$@"
done
if [ -n "$window" ]; then
  bash packaging/linux/appimage.sh "$bins/$(uname -m)" "$window" target/packages
fi
cd target/packages
set -- *.deb *.rpm
for appimage in *.AppImage; do
  [ ! -f "$appimage" ] || set -- "$@" "$appimage"
done
sha256sum "$@" > SHA256SUMS-linux
cat SHA256SUMS-linux
