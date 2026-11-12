#!/bin/bash
# Test the packaged systemd service inside a disposable Amazon Linux system.
# No host cgroup or Docker socket is mounted, and no host ports are published.
set -euo pipefail
packages=$(cd "$1" && pwd)
cli=$(cd "$2" && pwd)
here=$(cd "$(dirname "$0")" && pwd)
image=${ASAGIRI_CI_AL2023_IMAGE:-public.ecr.aws/amazonlinux/amazonlinux:2023@sha256:5c6ed3b38335b2aa424a045f1e564c8c508409ead0ff3f8bf5e62c31b94082cb}
name="asagiri-al2023-systemd-$$"
cid=''
key_mount=()
if [ -n "${3:-}" ]; then
  key=$(cd "$(dirname "$3")" && pwd)/$(basename "$3")
  key_mount=(--mount "type=bind,src=$key,dst=/key.asc,readonly")
fi
[ "$(docker info --format '{{.CgroupVersion}}')" = 2 ] || {
  echo "The Amazon Linux systemd check requires cgroup v2" >&2; exit 1;
}
cleanup() {
  result=$?
  [ -n "$cid" ] || return "$result"
  if [ "$result" -ne 0 ]; then
    docker logs --tail 60 "$cid" || true
    docker exec "$cid" journalctl -u asagiri.service -n 40 --no-pager || true
  fi
  docker rm -fv "$cid" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM
cid=$(docker run -d --name "$name" --label asagiri.test=amazon-linux-systemd \
  --privileged --cgroupns=private --tmpfs /run --tmpfs /tmp -v /var/lib \
  --mount "type=bind,src=$packages,dst=/packages,readonly" \
  --mount "type=bind,src=$cli,dst=/opt/docker,readonly" \
  --mount "type=bind,src=$here,dst=/checks,readonly" \
  "${key_mount[@]}" \
  -e container=docker -e "ASAGIRI_EXPECTED_VERSION=${ASAGIRI_EXPECTED_VERSION:-}" \
  "$image" bash /checks/test-amazon-systemd-container.sh)
ready=no
for _ in $(seq 120); do
  if docker exec "$name" bash -c 'test -x /usr/bin/systemctl && systemctl is-active --quiet multi-user.target' >/dev/null 2>&1; then ready=yes; break; fi
  [ "$(docker inspect --format '{{.State.Running}}' "$name")" = true ] || break
  sleep 2
done
[ "$ready" = yes ] || { echo "Amazon Linux systemd did not boot" >&2; exit 1; }
docker exec "$name" bash -c 'test "$(cat /proc/1/comm)" = systemd'
if [ -n "${3:-}" ]; then
  docker exec "$name" bash -c 'set -eu; rpm --import /key.asc; rpm -K /packages/*.rpm'
  docker exec -i "$name" python3 - <<'PY'
import configparser
from pathlib import Path
path = Path("/etc/dnf/dnf.conf")
config = configparser.ConfigParser()
config.read(path)
config["main"]["localpkg_gpgcheck"] = "True"
with path.open("w") as output:
    config.write(output)
PY
fi
docker exec "$name" bash /checks/test-host.sh /packages /opt/docker
