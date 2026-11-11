#!/usr/bin/env bash
# Exercise the real Linux daemon and Unix credentials in disposable containers.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${ASAGIRI_BIN_DIR:-"$ROOT/target/linux/release"}
RUN="asagiri-socket-$(date +%s)-$$"
VARIANTS="docker-group fallback fallback-gid managed custom-path"
# A group the daemon runs with in fallback-gid, and nobody is let in by.
DAEMON_GID=4242
cleanup() {
  for variant in $VARIANTS refused; do
    docker rm -fv "$RUN-$variant" >/dev/null 2>&1 || true
    docker volume rm "$RUN-$variant-state" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT
trap 'exit 130' INT TERM
# A /run/asagiri that is a link, or another user's, is refused: nothing is made
# through the link, and no socket in a directory someone else can change.
# So is a group lookup that fails (/etc/group is no file). The daemon that
# refuses exits with 1; one that started is killed instead.
docker volume create "$RUN-refused-state" >/dev/null
docker run --rm --privileged --name "$RUN-refused" -v "$BIN:/opt/asagiri:ro" -v "$RUN-refused-state:/var/lib/asagiri" docker:28.0.4-cli sh -c '
  fail() { echo "FAIL refused: $1"; exit 1; }
  refused() {
    timeout 30 /opt/asagiri/asagiri daemon >/dev/null 2>&1
    [ $? = 1 ] || fail "the daemon started with $1"
  }
  mkdir -p /run && mkdir -m 700 /tmp/elsewhere && ln -s /tmp/elsewhere /run/asagiri
  refused "/run/asagiri a symlink"
  [ -z "$(ls -A /tmp/elsewhere)" ] && [ "$(stat -c %a /tmp/elsewhere)" = 700 ] || fail "the symlink was followed"
  rm /run/asagiri && mkdir -m 700 /run/asagiri && chown 4242 /run/asagiri
  refused "/run/asagiri not owned by root"
  [ -z "$(ls -A /run/asagiri)" ] && [ "$(stat -c "%u %a" /run/asagiri)" = "4242 700" ] || fail "used a /run/asagiri not owned by root"
  # A group lookup that fails is not a missing group: nobody else is let in.
  rm -r /run/asagiri && mv /etc/group /etc/group.real && mkdir /etc/group
  refused "the group lookup failing"
  [ "$(stat -c "%u %a" /run/asagiri/docker.sock)" = "0 600" ] || fail "the socket is not root'"'"'s alone after a failed group lookup"
  rmdir /etc/group && mv /etc/group.real /etc/group
  echo "PASS refused: /run/asagiri as a symlink, and as a directory not owned by root; a failed group lookup"
'
for variant in $VARIANTS; do
  socket=/run/asagiri/docker.sock
  if [ "$variant" = custom-path ]; then socket=/run/asagiri-custom/docker.sock; fi
  docker volume create "$RUN-$variant-state" >/dev/null
  docker run -d --privileged --name "$RUN-$variant" \
    -e "SOCKET_TEST_VARIANT=$variant" -e "DAEMON_GID=$DAEMON_GID" \
    -v "$BIN:/opt/asagiri:ro" -v "$ROOT/tests:/tests:ro" \
    -v "$RUN-$variant-state:/var/lib/asagiri" \
    docker:28.0.4-cli sh -c '
      apk add --no-cache python3 >/dev/null
      umask 077
      case $SOCKET_TEST_VARIANT in
        fallback) sed -i "/^docker:/d" /etc/group ;;
        fallback-gid)
          sed -i "/^docker:/d" /etc/group
          exec python3 -c "import os; os.setgid($DAEMON_GID); os.execv(\"/opt/asagiri/asagiri\", [\"asagiri\", \"daemon\"])" ;;
        managed) export ASAGIRI_SOCKET_GROUP=root ;;
        custom-path)
          mkdir -m 755 /run/asagiri-custom
          export ASAGIRI_SOCKET=/run/asagiri-custom/docker.sock ;;
      esac
      exec /opt/asagiri/asagiri daemon
    ' >/dev/null
  ready=0
  for _ in $(seq 60); do
    if docker exec "$RUN-$variant" docker -H "unix://$socket" version >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 1
  done
  if [ "$ready" != 1 ]; then docker logs "$RUN-$variant"; exit 1; fi
  docker exec "$RUN-$variant" python3 /tests/socket-access.py "$variant" "$socket" "$DAEMON_GID"
done
