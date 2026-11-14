#!/bin/sh
# live-tests.sh <folder holding asagiri and asagiri-init>
#
# Starts that build as a private engine in a container (privileged, as the
# engine needs; no port is published, so nothing outside the machine can
# reach it) and runs the conformance scripts in tests/ against it, on
# busybox and on alpine. Prints one line per script, its complete output
# when it fails, and exits 1 when any failed.
set -u
bin=$(cd "$1" && pwd) || exit 2
src=$(cd "$(dirname "$0")/../.." && pwd)
name=asagiri-live-$$
sock=/run/asagiri/docker.sock
dock() { docker exec "$name" docker -H "unix://$sock" "$@"; }

docker run -d --privileged --name "$name" -e ASAGIRI_SOCKET="$sock" \
  -e ASAGIRI_DATA_DIR=/test-store -e ASAGIRI_TEST_ENGINE=1 \
  -v "$bin:/opt/asagiri:ro" -v "$src:/src:ro" -v /var/lib/asagiri -v /test-store \
  "${ASAGIRI_CI_LIVE_IMAGE:-docker:28.0.4-cli}" sh -c 'apk add -q --no-cache python3 bash curl && exec /opt/asagiri/asagiri daemon' >/dev/null || exit 2
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
  if [ "$rc" -ne 0 ]; then failed=$((failed + 1)); echo "$out" | sed 's/^/    /'; fi
}

run api-version
run engine-identity
run inspect-port-bindings alpine:3.21
run image-inspect-healthcheck
run image-rootfs-digests alpine:3.21
run layer-concurrency
for img in busybox:stable alpine:3.21; do
  run image-import "$img"
  run bind-inotify "$img"
done
for t in empty-response-status lifecycle-status client-command-compatibility user-resolution session-isolation reported-runtime-regressions archive-permissions archive-upload-ownership archive-configured-user archive-overwrite archive-directory-times http-body-framing archive-directory-contents \
  resource-limits restart-policy interactive-attach attach-http encoded-api-paths; do
  run "$t" alpine:3.21
done
for t in kill-during-start remove-during-start wait-code-after-removal network-bridge-icc network-bridge-mtu \
  network-endpoint-metadata archive-proc-symlink seccomp-profiles network-internal-gateway cpu-shares-weight \
  network-connect-dns network-late-default-route network-interface-numbers network-macvlan network-ipvlan; do
  for img in busybox:stable alpine:3.21; do run "$t" "$img"; done
done

run vm-recovery alpine:3.21

# Restart this disposable daemon process and its Linux host. A container
# with an always policy must come back with its writable layer intact.
# This covers Linux persistence; the macOS updater and launchd are separate.
restart_engine() {
  engine_id=$(dock info --format '{{.ID}}') || return 1
  # This script is evaluated by the container's shell.
  # shellcheck disable=SC2016
  cid=$(dock run -d --restart always --network none alpine:3.21 sh -c \
    'sid=$(cut -d" " -f6 /proc/$$/stat); [ "$sid" = "$$" ] || exit 42
     trap '"'"'trap "" TERM; kill -TERM 0; exit 0'"'"' TERM
     sleep 3600 & wait') || return 1
  dock exec "$cid" sh -c 'echo persisted > /restart-proof' || return 1
  i=0
  until dock exec "$cid" cat /restart-proof 2>/dev/null | grep -qx persisted; do
    i=$((i + 1)); [ "$i" -lt 50 ] || return 1; sleep 0.1
  done
  before=$(docker inspect --format '{{.State.Pid}}' "$name") || return 1
  docker restart -t 5 "$name" >/dev/null || return 1
  i=0
  until dock version >/dev/null 2>&1; do
    i=$((i + 1)); [ "$i" -lt 240 ] || return 1; sleep 0.5
  done
  after=$(docker inspect --format '{{.State.Pid}}' "$name") || return 1
  [ "$before" != "$after" ] || return 1
  docker exec -e "EXPECTED_ENGINE_ID=$engine_id" "$name" python3 /src/tests/engine-identity.py "$sock" || return 1
  i=0
  until dock exec "$cid" cat /restart-proof 2>/dev/null | grep -qx persisted; do
    i=$((i + 1)); [ "$i" -lt 100 ] || return 1; sleep 0.1
  done
  # #191: after a host/daemon restart, Compose stop can make a container
  # signal its entire process group. That group must exclude the daemon.
  dock stop "$cid" >/dev/null || return 1
  dock version >/dev/null || return 1
  dock rm -f "$cid" >/dev/null || return 1
}
if restart_engine; then
  echo "[0] daemon-restart: always policy and writable layer survive a new daemon process"
else
  failed=$((failed + 1)); echo "[1] daemon-restart: restart or persistence failed"
fi

run buildx-bootstrap

if [ "$failed" -ne 0 ]; then
  echo "$failed failed"; docker logs "$name" 2>&1; exit 1
fi
echo "all passed"
