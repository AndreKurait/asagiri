#!/bin/sh
# Builds the complete corresponding source archive for the guest kernel.
set -eu
out=${1:?"usage: scripts/ci/kernel-source.sh <output.tar.xz>"}
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
case $out in
  /* | [A-Za-z]:*) ;;
  *) out=$PWD/$out ;;
esac
kver=$(sed -n 's/^KVER="${KVER:-\([^}]*\)}"/\1/p' guest/kernel/build.sh)
[ -n "$kver" ] || { echo "could not read KVER from guest/kernel/build.sh" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 3 "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$kver.tar.xz" -o "$tmp/linux.tar.xz"
tar -xJf "$tmp/linux.tar.xz" -C "$tmp"
cd "$tmp/linux-$kver"
for patch in "$root"/guest/kernel/patches/*.patch; do
  patch -p1 -s < "$patch"
done
mkdir asagiri
cp "$root/guest/kernel/build.sh" "$root/guest/kernel/asagiri.config" "$root/guest/kernel/asagiri-x86_64.config" asagiri/
cp -R "$root/guest/kernel/patches" asagiri/
printf '%s\n' \
  "Asagiri guest kernel corresponding source" \
  "Upstream: https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$kver.tar.xz" \
  "All patches in asagiri/patches have already been applied to this tree." \
  "Build with asagiri/build.sh as described in the Asagiri repository." > asagiri/SOURCE
mkdir -p "$(dirname "$out")"
XZ_OPT=${XZ_OPT:--T0} tar -cJf "$out" -C "$tmp" "linux-$kver"
