#!/bin/sh
# Run with Alpine 3.21 loaded on a checkpoint-capable Asagiri engine.
set -eu
work=$(mktemp -d)
cid=''
cleanup() {
    if [ -n "$cid" ]; then docker rm -f "$cid" >/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
asagiri=${ASAGIRI_BIN:-asagiri}
cid=$(docker run -d --pull=never --network none alpine:3.21 sh -ec '
    read -r nonce </proc/sys/kernel/random/uuid
    exec 3>/tmp/heartbeat
    mkfifo /tmp/query /tmp/reply
    while read -r request </tmp/query; do
        printf "%s" "$request" >&3
        printf "%s\n" "$nonce" >/tmp/reply
    done
')
query() {
    docker exec "$cid" timeout 10 sh -ec 'printf "%s\n" "$1" >/tmp/query; cat /tmp/reply' sh "$1"
}
# Pair each request with a response. The nonce lives in the shell across capture.
for _ in 1 2 3 4 5; do
    if docker exec "$cid" test -p /tmp/reply; then break; fi
    sleep 1
done
docker exec "$cid" test -p /tmp/reply
nonce=$(query A)
echo "$nonce" | grep -Eq '^[[:xdigit:]]{8}(-[[:xdigit:]]{4}){3}-[[:xdigit:]]{12}$'
pid=$(docker inspect --format '{{.State.Pid}}' "$cid")
docker pause "$cid" >/dev/null
docker checkpoint create --leave-running "$cid" alpine-live
"$asagiri" checkpoint ls "$cid" | grep -Fx alpine-live
"$asagiri" checkpoint export "$cid" alpine-live "$work/checkpoint.tar"
test -n "$(find "$work/checkpoint.tar" -perm 0600 -print)"
tar -tf "$work/checkpoint.tar" >"$work/members"
grep -Eq '(^|/)inventory\.img$' "$work/members"
grep -Eq '(^|/)pstree\.img$' "$work/members"
grep -Eq '(^|/)pages-[0-9]+\.img$' "$work/members"
test "$(docker inspect --format '{{.State.Paused}}' "$cid")" = true
docker unpause "$cid" >/dev/null
test "$(query B)" = "$nonce"
test "$(docker exec "$cid" cat /tmp/heartbeat)" = AB
test "$(docker inspect --format '{{.State.Pid}}' "$cid")" = "$pid"
echo 'PASS checkpoint exported with memory pages and source retained its PID, shell nonce and open file offset'

docker rm -f "$cid" >/dev/null
cid=''
printf '%s\n' '{"Image":"alpine:3.21","Cmd":["true"]}' >"$work/create.json"
cid=$("$asagiri" checkpoint create-receiver "$work/checkpoint.tar" "$work/create.json")
test "$(docker inspect -f '{{.State.Status}} {{.HostConfig.NetworkMode}}' "$cid")" = 'created none'
"$asagiri" checkpoint import "$cid" received "$work/checkpoint.tar"
test "$("$asagiri" checkpoint ls "$cid")" = received
echo 'PASS offline receiver was created in none networking and imported the checkpoint'
