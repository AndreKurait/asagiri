#!/bin/bash
# PID 1 bootstrap for test-amazon-systemd.sh, never a host setup script.
set -euo pipefail
[ "$$" -eq 1 ] && [ "${container:-}" = docker ] || {
  echo "Run through test-amazon-systemd.sh in its disposable container" >&2; exit 1;
}
grep -q '^VERSION_ID="2023"$' /etc/os-release
grep -qx '0::/' /proc/self/cgroup
# The minimal Amazon Linux image does not include mount.
dnf -y install util-linux-core
# Keep kernel-wide settings read-only. Networking and the delegated cgroup
# hierarchy belong to this container's private namespaces.
mount -o remount,bind,ro /sys
mount --bind /proc/sys /proc/sys
mount -o remount,bind,ro /proc/sys
mount --bind /proc/sys/net /proc/sys/net
mount -o remount,bind,rw /proc/sys/net
mount --bind /dev/null /proc/sysrq-trigger
mount -o remount,bind,ro /proc/sysrq-trigger
dnf -y install systemd procps-ng util-linux shadow-utils findutils
# Docker configures this container's interface; networkd does not manage it.
# Real Amazon Linux hosts keep their normal network-online dependency.
ln -s /dev/null /etc/systemd/system/systemd-networkd-wait-online.service
exec /usr/lib/systemd/systemd --system --unit=multi-user.target --log-target=console
