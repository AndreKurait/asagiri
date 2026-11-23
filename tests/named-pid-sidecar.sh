#!/bin/sh
# Usage: sh tests/named-pid-sidecar.sh CONTEXT LOCAL_IMAGE
# Exercise the donor by name, confirm private isolation, then check it survives the sidecar.
set -eu
context=${1:?Docker context required}
image=${2:?locally available image with sh, sleep and readlink required}
docker_cli() { docker --context "$context" "$@"; }
name=asagiri-pid-test-$(uuidgen | tr 'A-Z' 'a-z')
cleanup() {
    docker_cli rm -f "$name-private" "$name-sidecar" "$name" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
namespace() { docker_cli exec "$name" readlink /proc/self/ns/pid; }
docker_cli run --detach --pull=never --network none --name "$name" "$image" sleep 120 >/dev/null
donor=$(namespace)
private=$(docker_cli run --rm --pull=never --network none --name "$name-private" "$image" readlink /proc/self/ns/pid)
case "$donor" in pid:\[*\]) ;; *) exit 1 ;; esac
case "$private" in pid:\[*\]) ;; *) exit 1 ;; esac
test "$private" != "$donor"
printf 'Donor %s, private control %s\n' "$donor" "$private"
joined=$(docker_cli run --rm --pull=never --network none --name "$name-sidecar" --pid "container:$name" "$image" readlink /proc/self/ns/pid)
test "$joined" = "$donor"
test "$(namespace)" = "$donor"
printf 'Sidecar joined %s and donor survived\n' "$joined"
