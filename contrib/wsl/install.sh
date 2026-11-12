#!/bin/sh
# Asagiri on Windows: run inside a WSL2 distro, as root, from the directory that
# holds the Linux build (cargo xtask windows; x86-64, or arm64 with --arch arm64):
#   wsl -d Ubuntu -u root sh ./install.sh
# Containers run on WSL2's Linux kernel. The daemon serves its unix socket
# only; asagiri-win.exe carries the Windows docker CLI's connections to it over
# `asagiri bridge` (see ../windows/asagiri.ps1). It is not served on TCP: WSL's
# localhost relay drops half-closes, which `docker run` needs.
set -eu
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
    printf '\n[boot]\ncommand = HOME=/var/lib/asagiri-daemon ASAGIRI_SOCKET=/var/lib/asagiri-daemon/.asagiri/run/docker.sock nohup /usr/local/bin/asagiri daemon >/var/log/asagiri.log 2>&1 &\n' >> /etc/wsl.conf
  fi
  pkill -f '/usr/local/bin/asagiri daemon' 2>/dev/null || true
  HOME=/var/lib/asagiri-daemon ASAGIRI_SOCKET=/var/lib/asagiri-daemon/.asagiri/run/docker.sock nohup /usr/local/bin/asagiri daemon >/var/log/asagiri.log 2>&1 &
fi
echo "Asagiri runs in WSL on /var/lib/asagiri-daemon/.asagiri/run/docker.sock"
