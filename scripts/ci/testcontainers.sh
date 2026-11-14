#!/bin/sh
# Test the real Java SDK against a fresh, private Asagiri engine. Only Maven
# dependencies are reused. No outer port, Docker socket or engine state is shared.
# Usage: testcontainers.sh BIN [MAVEN_CACHE]
set -eu
bin=$(cd "$1" && pwd)
src=$(cd "$(dirname "$0")/../.." && pwd)
cache=${2:-}
name="asagiri-testcontainers-$(python3 -c 'import uuid; print(uuid.uuid4().hex[:12])')"
sock=/run/asagiri/docker.sock
set --
if [ -n "$cache" ]; then
  mkdir -p "$cache"
  cache=$(cd "$cache" && pwd)
  set -- -v "$cache:/maven-cache" -e ASAGIRI_TEST_MAVEN_CACHE=/maven-cache
fi
cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then docker logs "$name" 2>&1 || true; fi
  docker rm -fv "$name" >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
image=${ASAGIRI_CI_LIVE_IMAGE:-docker:28.0.4-cli@sha256:2bccc79fc5fed23d876bf244fa5fbe78c54bf6b6145a9483604306fd797237d5}
docker run -d --privileged --name "$name" \
  -e ASAGIRI_SOCKET="$sock" -e ASAGIRI_DATA_DIR=/test-store -e ASAGIRI_TEST_ENGINE=1 \
  -v "$bin:/opt/asagiri:ro" -v "$src:/src:ro" -v /var/lib/asagiri -v /test-store \
  "$@" "$image" sh -c \
  'apk add -q --no-cache python3 openjdk17-jdk maven && exec /opt/asagiri/asagiri daemon' >/dev/null
[ "$(docker inspect -f '{{len .HostConfig.PortBindings}}' "$name")" = 0 ]
i=0
until docker exec "$name" docker -H "unix://$sock" version >/dev/null 2>&1; do
  i=$((i + 1))
  [ "$i" -lt 240 ] || { echo "The private Java test engine did not start" >&2; exit 1; }
  sleep 0.5
done
docker exec "$name" python3 /src/tests/testcontainers-integration.py "$sock"
