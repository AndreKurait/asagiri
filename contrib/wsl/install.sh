#!/bin/sh
# Asagiri on Windows: run inside a WSL2 distro, as root, from the directory that
# holds the Linux build (cargo xtask windows; x86-64, or arm64 with --arch arm64):
#   wsl -d Ubuntu -u root sh ./install.sh
# Containers run on WSL2's Linux kernel. The daemon serves its unix socket
# only; asagiri-win.exe carries the Windows docker CLI's connections to it over
# `asagiri bridge` (see ../windows/asagiri.ps1). It is not served on TCP: WSL's
# localhost relay drops half-closes, which `docker run` needs.
set -eu
boot_entry() {
  printf '\n[boot]\ncommand = HOME=/var/lib/asagiri-daemon ASAGIRI_SOCKET=/var/lib/asagiri-daemon/.asagiri/run/docker.sock nohup /usr/local/bin/asagiri daemon >/var/log/asagiri.log 2>&1 &\n'
}

if [ "${1:-}" = --remove-boot-entry ]; then
  conf=/etc/wsl.conf
  [ -f "$conf" ] || exit 0
  suffix=$(mktemp)
  trap 'rm -f "$suffix"' EXIT
  boot_entry > "$suffix"
  bytes=$(wc -c < "$suffix")
  if tail -c "$bytes" "$conf" | cmp -s - "$suffix"; then
    total=$(wc -c < "$conf")
    truncate -s "$((total - bytes))" "$conf"
  else
    # Later settings may belong to this boot section. Keep the section and
    # remove only our exact command immediately beneath its generated header.
    command=$(tail -n 1 "$suffix")
    offset=$(LC_ALL=C awk -v command="$command" '
      previous == "[boot]" && $0 == command { print bytes; exit }
      { previous = $0; bytes += length($0) + 1 }
    ' "$conf")
    if [ -n "$offset" ]; then
      conf=$(readlink -f "$conf")
      updated=$(mktemp "${conf}.XXXXXX")
      trap 'rm -f "$suffix" "$updated"' EXIT
      cp -p "$conf" "$updated"
      {
        head -c "$offset" "$conf"
        tail -c "+$((offset + ${#command} + 2))" "$conf"
      } > "$updated"
      mv -f "$updated" "$conf"
    fi
  fi
  exit 0
fi
here=$(cd "$(dirname "$0")" && pwd)
install -m 755 "$here/asagiri" "$here/asagiri-init" /usr/local/bin/
if [ -d /run/systemd/system ]; then
  cat > /etc/systemd/system/asagiri.service <<UNIT
[Unit]
Description=Asagiri container engine
After=network-online.target
[Service]
ExecStart=/usr/local/bin/asagiri daemon
Environment=HOME=/var/lib/asagiri-daemon
Environment=ASAGIRI_SOCKET=/var/lib/asagiri-daemon/.asagiri/run/docker.sock
Delegate=yes
# Container threads share this unit's budget; use per-container PID limits.
TasksMax=infinity
KillMode=mixed
Restart=on-failure
UMask=0027
[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable asagiri
  systemctl restart asagiri
else
  # No systemd in this distro: start Asagiri when WSL boots it.
  if ! grep -q 'asagiri daemon' /etc/wsl.conf 2>/dev/null; then
    boot_entry >> /etc/wsl.conf
  fi
  pkill -f '/usr/local/bin/asagiri daemon' 2>/dev/null || true
  export HOME=/var/lib/asagiri-daemon ASAGIRI_SOCKET=/var/lib/asagiri-daemon/.asagiri/run/docker.sock
  nohup /usr/local/bin/asagiri daemon </dev/null >/var/log/asagiri.log 2>&1 &
  daemon_pid=$!
  # Keep this WSL launch session alive until the daemon serves requests.
  attempts=0
  until /usr/local/bin/asagiri daemon-ping 2>/dev/null; do
    attempts=$((attempts + 1))
    if ! kill -0 "$daemon_pid" 2>/dev/null || [ "$attempts" -ge 15 ]; then
      echo "Asagiri did not become ready on $ASAGIRI_SOCKET; see /var/log/asagiri.log" >&2
      exit 1
    fi
    sleep 1
  done
fi
echo "Asagiri runs in WSL on /var/lib/asagiri-daemon/.asagiri/run/docker.sock"
