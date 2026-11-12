#!/bin/bash
# Exercise RPM installation and a real private engine on Amazon Linux 2023.
# Packages are local build inputs. An optional public key enforces RPM
# signatures when checking packages downloaded from the signed repository.
set -euo pipefail
packages=$(cd "$1" && pwd)
cli=$(cd "$2" && pwd)
helper=$(cd "$(dirname "$0")" && pwd)/test-amazon-container.sh
key=${3:-}
image=${ASAGIRI_CI_AL2023_IMAGE:-public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:5c6ed3b38335b2aa424a045f1e564c8c508409ead0ff3f8bf5e62c31b94082cb}
key_mount=()
if [ -n "$key" ]; then
  key=$(cd "$(dirname "$key")" && pwd)/$(basename "$key")
  key_mount=(--mount "type=bind,src=$key,dst=/key.asc,readonly")
fi
for name in asagiri-lite asagiri; do
  matches=("$packages/$name"-*.rpm)
  # The full package glob also sees asagiri-lite: require the version to
  # begin with a digit, as the package format does.
  if [ "$name" = asagiri ]; then matches=("$packages/asagiri-"[0-9]*.rpm); fi
  [ "${#matches[@]}" -eq 1 ] && [ -f "${matches[0]}" ] || {
    echo "Expected one native $name RPM in $packages" >&2; exit 1;
  }
  package=$(basename "${matches[0]}")
  echo "Amazon Linux 2023: $package"
  docker run --rm --name "asagiri-al2023-$name-$$" \
    --label asagiri.test=amazon-linux --privileged --tmpfs /run --tmpfs /tmp -v /var/lib \
    --mount "type=bind,src=$packages,dst=/packages,readonly" \
    --mount "type=bind,src=$cli,dst=/opt/docker,readonly" \
    --mount "type=bind,src=$helper,dst=/test-amazon-container.sh,readonly" \
    "${key_mount[@]}" \
    -e "ASAGIRI_PACKAGE=$package" -e "ASAGIRI_PACKAGE_NAME=$name" \
    -e "ASAGIRI_EXPECTED_VERSION=${ASAGIRI_EXPECTED_VERSION:-}" \
    "$image" bash /test-amazon-container.sh
done
