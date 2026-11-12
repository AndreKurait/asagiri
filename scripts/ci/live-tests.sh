#!/bin/sh
# live-tests.sh <folder holding asagiri and asagiri-init>
#
# Starts that build as a private engine in a container (privileged, as the
# engine needs; no port is published, so nothing outside the machine can
# reach it) and runs the conformance scripts in tests/ against it, on
# busybox and on alpine. Prints one line per script, the end of its output
# when it fails, and exits 1 when any failed.
set -u
bin=$(cd "$1" && pwd) || exit 2
src=$(cd "$(dirname "$0")/../.." && pwd)
name=asagiri-live-$$
sock=/run/asagiri/docker.sock
dock() { docker exec "$name" docker -H "unix://$sock" "$@"; }

docker run -d --privileged --name "$name" -e ASAGIRI_SOCKET="$sock" \
  -v "$bin:/opt/asagiri:ro" -v "$src:/src:ro" -v /var/lib/asagiri \
  docker:28.0.4-cli sh -c 'apk add -q --no-cache python3 bash curl && exec /opt/asagiri/asagiri daemon' >/dev/null || exit 2
trap 'docker rm -fv "$name" >/dev/null 2>&1' EXIT INT TERM

i=0
until dock version >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -gt 240 ]; then docker logs "$name" 2>&1 | tail -30; echo "the engine did not start"; exit 2; fi
  sleep 0.5
done
for img in busybox:stable alpine:3.21; do
  dock pull -q "$img" >/dev/null || { echo "could not pull $img"; exit 2; }
done

failed=0
run() { # run <script> [image]
  out=$(docker exec "$name" python3 "/src/tests/$1.py" "$sock" ${2:+"$2"} 2>&1); rc=$?
  echo "[$rc] $1${2:+ $2}: $(echo "$out" | tail -1 | cut -c1-200)"
  if [ "$rc" -ne 0 ]; then failed=$((failed + 1)); echo "$out" | tail -15 | sed 's/^/    /'; fi
}

run api-version
for img in busybox:stable alpine:3.21; do run image-import "$img"; done
for t in empty-response-status lifecycle-status archive-permissions http-body-framing archive-directory-contents \
  resource-limits restart-policy interactive-attach attach-http encoded-api-paths; do
  run "$t" alpine:3.21
done
for t in kill-during-start remove-during-start wait-code-after-removal network-bridge-icc network-bridge-mtu \
  network-endpoint-metadata archive-proc-symlink seccomp-profiles network-internal-gateway cpu-shares-weight \
  network-connect-dns network-late-default-route network-interface-numbers network-macvlan network-ipvlan; do
  for img in busybox:stable alpine:3.21; do run "$t" "$img"; done
done

if [ "$failed" -ne 0 ]; then
  echo "$failed failed"; docker logs "$name" 2>&1 | tail -40; exit 1
fi
echo "all passed"
