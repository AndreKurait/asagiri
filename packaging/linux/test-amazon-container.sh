#!/bin/bash
# Invoked only inside the disposable Amazon Linux test container.
set -euo pipefail
grep -q '^VERSION_ID="2023"$' /etc/os-release
signature=()
if [ -f /key.asc ]; then
  rpm --import /key.asc
  signature=(--setopt=localpkg_gpgcheck=1)
fi
dnf -y "${signature[@]}" install "/packages/$ASAGIRI_PACKAGE" util-linux
rpm -V "$ASAGIRI_PACKAGE_NAME"
cli=/opt/docker/docker
if [ "$ASAGIRI_PACKAGE_NAME" = asagiri ]; then
  cli=/usr/libexec/asagiri/bin/docker
  docker --version
  docker compose version
  docker buildx version
fi
mkdir -p /run/asagiri /var/lib/asagiri-daemon
chmod 755 /run/asagiri
ASAGIRI_SOCKET=/run/asagiri/docker.sock ASAGIRI_SOCKET_GROUP=asagiri \
  HOME=/var/lib/asagiri-daemon /usr/bin/asagiri daemon >/tmp/asagiri.log 2>&1 &
engine=$!
trap 'kill "$engine" 2>/dev/null || true' EXIT
for _ in $(seq 30); do
  [ ! -S /run/asagiri/docker.sock ] || break
  sleep 1
done
[ -S /run/asagiri/docker.sock ] || { cat /tmp/asagiri.log; exit 1; }
useradd -m ec2-user
access=0
/usr/bin/asagiri host-access ec2-user || access=$?
[ "$access" -eq 0 ] || [ "$access" -eq 10 ]
runuser -u ec2-user -- "$cli" context create asagiri --docker host=unix:///run/asagiri/docker.sock
version=$(runuser -u ec2-user -- "$cli" --context asagiri version --format '{{.Server.Version}}')
[ -z "$ASAGIRI_EXPECTED_VERSION" ] || [ "$version" = "$ASAGIRI_EXPECTED_VERSION" ]
runuser -u ec2-user -- timeout -s KILL 120 "$cli" --context asagiri run --rm alpine:3.21 echo AL2023-OK
kill "$engine"
wait "$engine" || true
trap - EXIT
dnf -y remove "$ASAGIRI_PACKAGE_NAME"
test ! -e /usr/bin/asagiri
test ! -e /usr/lib/systemd/system/asagiri.service
echo "PASS Amazon Linux 2023: install, user access, real container, removal"
