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
# Run the same archive cases on the runner's Docker engine for comparison.
python3 "$src/tests/layer-hardlinks.py" /var/run/docker.sock --reference-docker || exit 1
# Establish Docker's allowed serialization behavior before testing the private
# Asagiri build. The fixture itself never pulls or touches unowned containers.
docker image inspect alpine:3.21 >/dev/null 2>&1 || docker pull -q alpine:3.21 >/dev/null || exit 2
python3 "$src/tests/shared-root.py" /var/run/docker.sock alpine:3.21 --reference-docker || exit 1
name=asagiri-live-$$
sock=/run/asagiri/docker.sock
dock() { docker exec "$name" docker -H "unix://$sock" "$@"; }

# Run the identical many-layer fixture on the runner's Docker daemon.
python3 "$src/tests/many-layers.py" /var/run/docker.sock alpine:3.21 || exit 2
# The same read-only fixture runs against the runner's Docker daemon first.
python3 "$src/tests/distribution.py" /var/run/docker.sock --reference-docker || exit 2
python3 "$src/tests/image-label-inheritance.py" /var/run/docker.sock || exit 2
python3 "$src/tests/volume-owner.py" /var/run/docker.sock alpine:3.21 --reference-docker || exit 2
python3 "$src/tests/start-exec-image-user.py" /var/run/docker.sock alpine:3.21 --reference-docker || exit 2
python3 "$src/tests/user-resolution.py" /var/run/docker.sock alpine:3.21 || exit 2
python3 "$src/tests/daemon-time.py" /var/run/docker.sock --reference-docker || exit 2

# IPv6 probes need an enabled private host namespace. Docker may otherwise
# initialize this namespace with IPv6 disabled when its bridge is IPv4-only.
docker run -d --privileged --name "$name" -e ASAGIRI_SOCKET="$sock" \
  --sysctl net.ipv6.conf.all.disable_ipv6=0 --sysctl net.ipv6.conf.default.disable_ipv6=0 \
  -e ASAGIRI_DATA_DIR=/test-store -e ASAGIRI_TEST_ENGINE=1 \
  -v "$bin:/opt/asagiri:ro" -v "$src:/src:ro" -v /var/lib/asagiri -v /test-store \
  "${ASAGIRI_CI_LIVE_IMAGE:-docker:28.0.4-cli}" sh -c \
  'apk add -q --no-cache python3 py3-pip bash curl strace openssl util-linux &&
   python3 -m venv /tmp/asagiri-mcp-sdk &&
   /tmp/asagiri-mcp-sdk/bin/pip install -q --require-hashes -r /src/tests/mcp-sdk-requirements.txt &&
   exec /opt/asagiri/asagiri daemon' >/dev/null || exit 2
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
run() { # run <script> [image] [options]
  test_script=$1
  shift
  interpreter=python3
  [ "$test_script" != mcp-sdk ] || interpreter=/tmp/asagiri-mcp-sdk/bin/python
  out=$(docker exec "$name" "$interpreter" "/src/tests/$test_script.py" "$sock" "$@" 2>&1); rc=$?
  case "$test_script" in
    failed-start-veth|remove-responsive|volume-owner|shared-root|run-options|start-exec-image-user|daemon-time)
      echo "[$rc] $test_script${*:+ $*}: structured result"
      printf '%s\n' "$out"
      ;;
    *)
      echo "[$rc] $test_script${*:+ $*}: $(echo "$out" | tail -1 | cut -c1-200)"
      if [ "$rc" -ne 0 ]; then printf '%s\n' "$out" | sed 's/^/    /'; fi
      ;;
  esac
  if [ "$rc" -ne 0 ]; then failed=$((failed + 1)); fi
}

run api-version --cold
run engine-identity
run engine-power alpine:3.21
run exec-errors alpine:3.21
run http-connection-close
run embedded-dns-port alpine:3.21
run inspect-port-bindings alpine:3.21
run run-options alpine:3.21
run start-exec-image-user alpine:3.21
run daemon-time
run distribution
run image-label-inheritance
run many-layers alpine:3.21
run image-inspect-healthcheck
run image-rootfs-digests alpine:3.21
run layer-hardlinks
run image-load-completion
run volume-owner alpine:3.21
run image-tag-retention alpine:3.21
run layer-concurrency
run container-addresses
run network-allocation alpine:3.21
run network-dns-reuse alpine:3.21
run failed-start-veth alpine:3.21
run cleanup-responsive alpine:3.21
run cleanup-responsive alpine:3.21 --copy
run cleanup-responsive alpine:3.21 --crash-agent
run remove-cleanup-retry alpine:3.21
run remove-responsive alpine:3.21
run api-connections alpine:3.21
run archive-completion alpine:3.21
run shared-root alpine:3.21
run init-descriptors alpine:3.21
run device-filter-failure alpine:3.21
run overlay-copyup alpine:3.21
run kind-network alpine:3.21
run mcp-integration alpine:3.21
run mcp-sdk alpine:3.21
run browser-integration alpine:3.21
for img in busybox:stable alpine:3.21; do
  run image-import "$img"
  run bind-inotify "$img"
  run start-exec-readiness "$img"
done
for t in empty-response-status lifecycle-status client-command-compatibility user-resolution session-isolation reported-runtime-regressions archive-permissions archive-stopped-mounts archive-upload-ownership archive-configured-user archive-overwrite archive-directory-times http-body-framing archive-directory-contents \
  resource-limits restart-policy interactive-attach attach-http encoded-api-paths log-readiness; do
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
  docker exec "$name" python3 /src/tests/image-label-inheritance.py "$sock" --prepare-restart /tmp/image-label-restart.json || return 1
  docker exec "$name" python3 /src/tests/image-tag-retention.py "$sock" alpine:3.21 --prepare-restart /tmp/image-retention-restart.json || return 1
  before=$(docker inspect --format '{{.State.Pid}}' "$name") || return 1
  docker restart -t 5 "$name" >/dev/null || return 1
  i=0
  until dock version >/dev/null 2>&1; do
    i=$((i + 1)); [ "$i" -lt 240 ] || return 1; sleep 0.5
  done
  docker exec "$name" python3 /src/tests/image-label-inheritance.py "$sock" --verify-restart /tmp/image-label-restart.json || return 1
  docker exec "$name" python3 /src/tests/image-tag-retention.py "$sock" --verify-restart /tmp/image-retention-restart.json || return 1
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

# Cold bootstrap must precede probes that start the managed BuildKit backend.
run buildx-bootstrap
run registry-config
run buildkit-driver alpine:3.21
run builder-idle alpine:3.21
run kind-cluster alpine:3.21

if [ "$failed" -ne 0 ]; then
  echo "$failed failed"; docker logs "$name" 2>&1; exit 1
fi
echo "all passed"
