#!/bin/sh
# Usage: sh tests/named-pid-sidecar.sh CONTEXT LOCAL_IMAGE
# Exercise the donor by name, confirm private isolation, then check it survives the sidecar.
set -eu
context=${1:?Docker context required}
image=${2:?locally available image with sh, sleep and readlink required}
docker_cli() { docker --context "$context" "$@"; }
name=asagiri-pid-test-$(uuidgen | tr 'A-Z' 'a-z')
cleanup() {
    docker_cli rm -f "$name-private" "$name-sidecar" "$name-stopped" "$name" >/dev/null 2>&1 || true
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
after=$(docker_cli run --rm --pull=never --network none --name "$name-private" "$image" readlink /proc/self/ns/pid)
test "$after" != "$donor"
docker_cli create --network none --name "$name-stopped" --pid "container:$name" "$image" true >/dev/null
docker_cli stop -t 1 "$name" >/dev/null
if output=$(docker_cli start "$name-stopped" 2>&1); then
    echo 'Sidecar started after its donor stopped' >&2
    exit 1
fi
case "$output" in *running*) ;; *) echo "$output" >&2; exit 1 ;; esac
printf 'Subsequent private run stayed isolated and a stopped donor was rejected\n'
