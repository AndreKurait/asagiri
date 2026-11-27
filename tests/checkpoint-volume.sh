#!/bin/sh
# Run with Alpine 3.21 loaded on a checkpoint-capable Asagiri engine.
set -eu
work=$(mktemp -d)
volume=''
writer=''
peer=''
cleanup() {
    if [ -n "$peer" ]; then docker rm -f "$peer" >/dev/null || true; fi
    if [ -n "$writer" ]; then docker rm -f "$writer" >/dev/null || true; fi
    if [ -n "$volume" ]; then docker volume rm "$volume" >/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
asagiri=${ASAGIRI_BIN:-asagiri}
volume=$(docker volume create)
writer=$(docker run -d --pull=never --network none -v "$volume:/data" -v "$volume:/mirror:ro" alpine:3.21 sh -ec '
    read -r nonce </proc/sys/kernel/random/uuid
    exec 3>/data/state
    chmod 640 /data/state
    ln /data/state /data/hard
    ln -s state /data/link
    mkfifo /tmp/query /tmp/reply
    while read -r value </tmp/query; do
        printf "%s" "$value" >&3
        printf "%s\n" "$nonce" >/tmp/reply
    done
')
peer=$(docker run -d --pull=never --network none -v "$volume:/shared" alpine:3.21 sleep 3600)
query() {
    docker exec "$writer" timeout 10 sh -ec 'printf "%s\n" "$1" >/tmp/query; cat /tmp/reply' sh "$1"
}
for _ in 1 2 3 4 5; do
    if docker exec "$writer" test -p /tmp/reply; then break; fi
    sleep 1
done
docker exec "$writer" test -p /tmp/reply
nonce=$(query A)
test "${#nonce}" -eq 36
pid=$(docker inspect --format '{{.State.Pid}}' "$writer")
docker pause "$writer" >/dev/null
if docker checkpoint create --leave-running "$writer" shared >"$work/response" 2>&1; then exit 1; fi
grep -F 'another active writer' "$work/response"
test -z "$("$asagiri" checkpoint ls "$writer")"
docker pause "$peer" >/dev/null
docker checkpoint create --leave-running "$writer" shared
"$asagiri" checkpoint ls "$writer" | grep -Fx shared
docker unpause "$peer" >/dev/null
docker unpause "$writer" >/dev/null
test "$(query B)" = "$nonce"
test "$(docker inspect --format '{{.State.Pid}}' "$writer")" = "$pid"
test "$(docker exec "$peer" cat /shared/state)" = AB
"$asagiri" checkpoint export "$writer" shared "$work/checkpoint.tar"
test -n "$(find "$work/checkpoint.tar" -perm 0600 -print)"
tar -tf "$work/checkpoint.tar" >"$work/list"
grep -Eq '(^|/)pages-[0-9]+\.img$' "$work/list"
test "$(grep -Fxc "volumes/$volume/data.tar" "$work/list")" -eq 1
if grep -Eq 'volume-hook|\.partial$' "$work/list"; then exit 1; fi
tar -xOf "$work/checkpoint.tar" volume-mounts.json >"$work/mounts"
grep -Fq "\"name\":\"$volume\",\"target\":\"/data\",\"readOnly\":false" "$work/mounts"
grep -Fq "\"name\":\"$volume\",\"target\":\"/mirror\",\"readOnly\":true" "$work/mounts"
tar -xOf "$work/checkpoint.tar" "volumes/$volume/data.tar" >"$work/volume.tar"
mkdir "$work/snapshot"
tar -xf "$work/volume.tar" -C "$work/snapshot"
test "$(cat "$work/snapshot/state")" = A
test "$work/snapshot/state" -ef "$work/snapshot/hard"
test "$(readlink "$work/snapshot/link")" = state
test -n "$(find "$work/snapshot/state" -perm 0640 -print)"
echo 'PASS named volume captured once with both mount modes, open file offset, links and checkpoint-time bytes'
