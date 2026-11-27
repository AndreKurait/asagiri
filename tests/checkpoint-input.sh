#!/usr/bin/env bash
# Run against a dedicated Asagiri engine with alpine:3.21 already loaded.
set -euo pipefail
umask 077
asagiri=${ASAGIRI_BIN:-asagiri}
work=$(mktemp -d)
containers=()
client=
fence=
owner=
cleanup() {
    if [[ -n $client ]]; then kill -KILL "$client" 2>/dev/null || true; wait "$client" 2>/dev/null || true; fi
    if [[ -n $fence ]]; then "$asagiri" checkpoint release "$fence" "$owner" >/dev/null 2>&1 || true; fi
    for cid in ${containers[@]+"${containers[@]}"}; do docker rm -f "$cid" >/dev/null 2>&1 || true; done
    rm -rf "$work"
}
trap cleanup EXIT
cd "$work"
# Distinct offsets expose duplicated, missing or reordered bytes between the kernel pipe and agent queue.
awk 'BEGIN {for(i=0;i<16384;i++) printf "%08x\n",i}' >expected
source_id=$(curl -fsS --unix-socket "${ASAGIRI_SOCKET:?}" -H 'Content-Type: application/json' \
    -d '{"Image":"alpine:3.21","Cmd":["sleep","300"],"OpenStdin":true,"StdinOnce":true,"AttachStdin":true,"HostConfig":{"NetworkMode":"none"}}' \
    http://localhost/containers/create | sed -E 's/.*"Id":"([0-9a-f]+)".*/\1/')
[[ $source_id =~ ^[[:xdigit:]]{64}$ ]]
containers+=("$source_id")
docker start "$source_id" >/dev/null
cat expected | docker attach --sig-proxy=false "$source_id" >attach.out 2>attach.err &
client=$!
sleep 1
kill -0 "$client" 2>/dev/null || { cat attach.err >&2; exit 1; }
# Wait for the pending EOF to reach the guest before selecting a capture boundary.
for attempt in {1..5}; do
    name=backlog-$attempt
    owner=$source_id
    fence=$name
    "$asagiri" checkpoint capture-group "$name" "$source_id" >/dev/null
    rm -f source.tar
    "$asagiri" checkpoint export "$source_id" "$name" source.tar
    "$asagiri" checkpoint release "$name" "$source_id" >/dev/null
    fence=
    docker unpause "$source_id" >/dev/null
    if tar -xOf source.tar stdio.json | grep -q '"close_after_input":true'; then break; fi
done
tar -xOf source.tar stdio.json | grep -q '"close_after_input":true'
tar -xOf source.tar stdin.bin | cmp - expected
docker rm -f "$source_id" >/dev/null
wait "$client" || true
client=
containers=()
cat >receiver.json <<'JSON'
{"Image":"alpine:3.21","Cmd":["sh","-ec","while [ ! -e /tmp/read ]; do sleep .05; done; cat >/tmp/input && touch /tmp/eof; sleep 120"]}
JSON
receiver=$("$asagiri" checkpoint create-receiver source.tar receiver.json)
containers+=("$receiver")
"$asagiri" checkpoint import "$receiver" saved source.tar
if "$asagiri" checkpoint restore-stdin "$receiver" saved 2>error; then exit 1; fi
grep -q 'running receiver' error
docker start "$receiver" >/dev/null
"$asagiri" checkpoint restore-stdin "$receiver" saved
"$asagiri" checkpoint restore-stdin "$receiver" saved
if "$asagiri" checkpoint restore-stdin "$receiver" other 2>error; then exit 1; fi
grep -q 'another checkpoint' error
# A second handoff while replay is backpressured must preserve the unread suffix and EOF.
owner=$receiver
fence=second
"$asagiri" checkpoint capture-group second "$receiver" >/dev/null
"$asagiri" checkpoint export "$receiver" second second.tar
tar -xOf second.tar stdin.bin | cmp - expected
tar -xOf second.tar stdio.json | grep -q '"close_after_input":true'
"$asagiri" checkpoint release second "$receiver" >/dev/null
fence=
docker unpause "$receiver" >/dev/null
docker exec "$receiver" touch /tmp/read
for attempt in {1..100}; do
    if docker exec "$receiver" test -f /tmp/eof; then break; fi
    sleep .1
done
docker exec "$receiver" test -f /tmp/eof
docker exec "$receiver" cat /tmp/input | cmp - expected
"$asagiri" checkpoint restore-stdin "$receiver" saved
# A different lifetime must accept ordinary input without replaying the old checkpoint.
docker restart "$receiver" >/dev/null
printf live | docker attach --sig-proxy=false "$receiver" >attach.out 2>attach.err &
client=$!
for attempt in {1..100}; do
    if [[ $(docker exec "$receiver" cat /tmp/input) == live ]]; then break; fi
    sleep .1
done
test "$(docker exec "$receiver" cat /tmp/input)" = live
if "$asagiri" checkpoint restore-stdin "$receiver" saved 2>error; then exit 1; fi
grep -q 'unused non-TTY input pipe' error
echo 'PASS checkpoint replay delivered 147456 bytes once, preserved a second handoff under backpressure and closed stdin'
