#!/bin/bash
# Assemble the x86_64 desktop deb on its oldest supported Ubuntu release.
set -euo pipefail
packages=$(cd "$1" && pwd)
version=$2
deb_version=${version/-/\~}
inputs=("$packages"/asagiri_*_amd64.deb)
test "${#inputs[@]}" = 1
test "$(dpkg-deb -f "${inputs[0]}" Version)" = "$deb_version"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
sudo apt-get update -qq
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev xvfb xdotool dbus-x11
dpkg-deb -x "${inputs[0]}" "$work/input"
mkdir "$work/native" "$work/cli"
cp "$work/input/usr/bin/"asagiri{,-init} "$work/native/"
git rev-parse HEAD > "$work/native/SOURCE_COMMIT"
cp "$work/input/usr/libexec/asagiri/bin/docker" "$work/input/usr/libexec/asagiri/cli-plugins/"docker-* "$work/cli/"
cargo build --locked --release -p asagiri --features window
bash packaging/linux/build.sh "$work/native" "$work/out" x86_64 "$work/cli" "$PWD/target/release/asagiri"
asset=$(basename "${inputs[0]}")
sudo apt-get install -y "$work/out/$asset"
test "$(dpkg-query -W -f='${Version}' asagiri)" = "$deb_version"
test "$(/usr/lib/asagiri/asagiri-window --version)" = "Asagiri $version"
xvfb-run -a dbus-run-session -- bash -ec '
  asagiri ui & pid=$!
  trap '\''kill "$pid" 2>/dev/null || true'\'' EXIT
  window=$(timeout 60 xdotool search --sync --onlyvisible --pid "$pid" --name Asagiri | head -1)
  test -n "$window"
  xdotool getwindowgeometry "$window"
'
cp "$work/out/$asset" "${inputs[0]}"
