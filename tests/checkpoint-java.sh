#!/usr/bin/env bash
# Run against a checkpoint-capable Asagiri engine with the Java 21 image loaded.
set -euo pipefail
work=$(mktemp -d)
cid=''
cleanup() {
  if [[ -n "$cid" ]]; then docker rm -f "$cid" >/dev/null || true; fi
  rm -rf "$work"
}
trap cleanup EXIT
javac --release 21 -d "$work" "$(dirname "$0")/fixtures/checkpoint/HeapProbe.java"
cid=$(docker create --pull=never --network none --memory 424m --entrypoint java eclipse-temurin:21-jre-alpine -Xmx32m -cp /tmp HeapProbe)
docker cp "$work/HeapProbe.class" "$cid:/tmp/HeapProbe.class"
docker start "$cid" >/dev/null
reply=''
for ((attempt=0; attempt<60; attempt++)); do
  if reply=$(docker exec "$cid" nc -w 2 127.0.0.1 15060); then break; fi
  sleep 0.25
done
[[ $reply =~ ^([0-9a-f-]{36})\ 1$ ]]
nonce=${BASH_REMATCH[1]}
docker pause "$cid" >/dev/null
docker cp "$cid:/tmp/heartbeat" "$work/before"
docker checkpoint create --leave-running "$cid" java-live
docker checkpoint ls "$cid" | grep -Fx java-live
docker cp "$cid:/tmp/heartbeat" "$work/after"
[[ $(wc -c < "$work/before") -eq 8 ]]
cmp "$work/before" "$work/after"
[[ $(docker inspect --format '{{.State.Paused}}' "$cid") == true ]]
docker unpause "$cid" >/dev/null
[[ $(docker exec "$cid" nc -w 2 127.0.0.1 15060) == "$nonce 2" ]]
printf 'PASS Java heap, open file and internal socket survived pause, live capture and unpause\n'
