#!/bin/sh
# Run with Alpine 3.21 loaded on a checkpoint-capable Asagiri engine.
set -eu
work=$(mktemp -d)
volume=''
writer=''
peer=''
receiver=''
occupied=''
cleanup() {
    if [ -n "$receiver" ]; then docker rm -f "$receiver" >/dev/null || true; fi
    if [ -n "$occupied" ]; then docker rm -f "$occupied" >/dev/null || true; fi
    if [ -n "$peer" ]; then docker rm -f "$peer" >/dev/null || true; fi
    if [ -n "$writer" ]; then docker rm -f "$writer" >/dev/null || true; fi
    if [ -n "$volume" ]; then docker volume rm "$volume" >/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
asagiri=${ASAGIRI_BIN:-asagiri}
volume=$(docker volume create)
writer=$(docker run -d --pull=never --network none -v "$volume:/data" -v "$volume:/mirror:ro" alpine:3.21 sh -ec '
    printf before >/etc/alpine-release
    rm /etc/issue
    rm -rf /etc/crontabs
    mkdir /etc/crontabs /checkpoint-files
    printf fresh >/etc/crontabs/new
    printf linked >/checkpoint-files/a
    chmod 640 /checkpoint-files/a
    ln /checkpoint-files/a /checkpoint-files/b
    ln -s a /checkpoint-files/symlink
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
docker exec "$writer" sh -ec 'printf after >/etc/alpine-release; printf later >/checkpoint-files/a'
test "$(docker inspect --format '{{.State.Pid}}' "$writer")" = "$pid"
test "$(docker exec "$peer" cat /shared/state)" = AB
"$asagiri" checkpoint export "$writer" shared "$work/checkpoint.tar"
test -n "$(find "$work/checkpoint.tar" -perm 0600 -print)"
tar -tf "$work/checkpoint.tar" >"$work/list"
grep -Eq '(^|/)pages-[0-9]+\.img$' "$work/list"
test "$(grep -Fxc "volumes/$volume/data.tar" "$work/list")" -eq 1
if grep -Eq 'filesystem-hook|volume-hook|\.partial$' "$work/list"; then exit 1; fi
tar -xOf "$work/checkpoint.tar" rootfs/manifest.json | grep -Fq 'application/vnd.oci.image.layer.v1.tar'
tar -xOf "$work/checkpoint.tar" rootfs/layer.tar >"$work/layer.tar"
tar -tf "$work/layer.tar" >"$work/paths"
grep -Eq '^(\./)?etc/\.wh\.issue$' "$work/paths"
grep -Eq '^(\./)?etc/crontabs/\.wh\.\.wh\.\.opq$' "$work/paths"
if grep -Eq '^(\./)?(data|mirror)/state$' "$work/paths"; then exit 1; fi
mkdir "$work/writable"
# Extract only the ordinary fixture files. The layer can also contain FIFOs
# and container-owned system paths, which the host test does not need.
tar -xf "$work/layer.tar" -C "$work/writable" ./etc/alpine-release ./etc/crontabs/new ./checkpoint-files
test "$(cat "$work/writable/etc/alpine-release")" = before
test "$(cat "$work/writable/etc/crontabs/new")" = fresh
test "$(cat "$work/writable/checkpoint-files/a")" = linked
test "$work/writable/checkpoint-files/a" -ef "$work/writable/checkpoint-files/b"
test "$(readlink "$work/writable/checkpoint-files/symlink")" = a
test -n "$(find "$work/writable/checkpoint-files/a" -perm 0640 -print)"
echo 'PASS writable layer retained checkpoint-time bytes, links, file deletion and opaque directory'
receiver=$(docker create --pull=never --network none alpine:3.21 sh -ec '
    test "$(cat /etc/alpine-release)" = before
    test ! -e /etc/issue && test ! -e /etc/crontabs/root
    test "$(cat /etc/crontabs/new)" = fresh
    test /checkpoint-files/a -ef /checkpoint-files/b
    test "$(cat /checkpoint-files/b)" = linked
    test "$(readlink /checkpoint-files/symlink)" = a
    test "$(stat -c %a /checkpoint-files/a)" = 640
    test -p /tmp/query && test -p /tmp/reply
    printf restored >/checkpoint-files/b
    test "$(cat /checkpoint-files/a)" = restored')
"$asagiri" checkpoint import "$receiver" received "$work/checkpoint.tar"
docker cp "$receiver:/etc/issue" "$work/before-restore"
test -s "$work/before-restore"
"$asagiri" checkpoint restore-rootfs "$receiver" received
test "$(docker inspect -f '{{.State.Status}}' "$receiver")" = created
docker cp "$receiver:/checkpoint-files/a" "$work/from-receiver"
test "$(cat "$work/from-receiver")" = linked
"$asagiri" checkpoint restore-rootfs "$receiver" received
docker start -a "$receiver"
test "$(docker inspect -f '{{.State.ExitCode}}' "$receiver")" = 0
if "$asagiri" checkpoint restore-rootfs "$receiver" received >"$work/status" 2>&1; then exit 1; fi
grep -Fq 'freshly created receiver' "$work/status"
occupied=$(docker create --pull=never --network none alpine:3.21 true)
printf keep >"$work/keep"
docker cp "$work/keep" "$occupied:/keep"
"$asagiri" checkpoint import "$occupied" received "$work/checkpoint.tar"
if "$asagiri" checkpoint restore-rootfs "$occupied" received >"$work/status" 2>&1; then exit 1; fi
grep -Fq 'receiver writable layer is not empty' "$work/status"
docker cp "$occupied:/keep" "$work/retained"
cmp "$work/keep" "$work/retained"
echo 'PASS ordinary receiver start saw restored writable files, OCI deletions, links and FIFOs; a used writable layer was preserved'
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

# Keep a fresh, long-lived workload for the capture admission and retry checks.
docker rm -f "$writer" >/dev/null
writer=$(docker run -d --pull=never --network none alpine:3.21 sleep 600)
retry_pid=$(docker inspect --format '{{.State.Pid}}' "$writer")
# Independent execs have their own process tree. Capture must leave the
# published set intact while one can still write the root filesystem.
docker exec -d "$writer" sh -ec 'touch /tmp/exec-started; while [ ! -e /tmp/exec-finish ]; do sleep .1; done'
for _ in 1 2 3 4 5; do
    if docker exec "$writer" test -f /tmp/exec-started; then break; fi
    sleep 1
done
docker exec "$writer" test -f /tmp/exec-started
if docker checkpoint create --leave-running "$writer" busy >"$work/response" 2>&1; then exit 1; fi
grep -F 'active container exec' "$work/response"
test -z "$("$asagiri" checkpoint ls "$writer")"
docker exec "$writer" touch /tmp/exec-finish
# OCI reserves .wh. names, including ordinary files with this prefix.
# Wait for the detached exec to finish before checking the hook failure.
docker exec "$writer" sh -ec 'touch /tmp/.wh.literal'
for _ in 1 2 3 4 5; do
    if docker checkpoint create --leave-running "$writer" retry >"$work/response" 2>&1; then exit 1; fi
    if ! grep -Fq 'active container exec' "$work/response"; then break; fi
    sleep 1
done
cat "$work/response"
grep -Fq 'OCI reserves writable layer name' "$work/response"
test "$(docker inspect --format '{{.State.Pid}} {{.State.Running}} {{.State.Paused}}' "$writer")" = "$retry_pid true false"
test -z "$("$asagiri" checkpoint ls "$writer")"
docker exec "$writer" rm /tmp/.wh.literal
docker checkpoint create --leave-running "$writer" retry
"$asagiri" checkpoint ls "$writer" | grep -Fx retry
echo 'PASS concurrent exec and unrepresentable layer withheld the checkpoint and the same name succeeded on retry'
