# Asagiri on a Linux host as an RPM: `asagiri` (engine + docker CLI, Compose,
# buildx) or `asagiri-lite` (engine only), chosen by packaging/linux/build.sh
# through asagiri_name and with_cli. Static (musl) binaries: no runtime
# dependencies, no debuginfo. With with_window, `asagiri` also carries the
# window (asagiri built against GTK 3 and WebKitGTK 4.1), which needs those.
%global debug_package %{nil}
%global __strip /bin/true
%global __brp_mangle_shebangs %{nil}

Name:           %{asagiri_name}
Version:        %{asagiri_version}
Release:        1%{?dist}
Summary:        Tiny, fast container runtime (Docker API)%{!?with_cli:, engine only}
License:        Apache-2.0
URL:            https://andrekurait.github.io/asagiri
Source0:        asagiri
Source1:        asagiri-init
Source2:        asagiri.service
Source3:        LICENSE
Source4:        NOTICE
Source18:       dev.asagiri.host-access.policy
Source20:       asagiri-nm.conf
%if 0%{?with_cli}
Source5:        docker
Source6:        docker-compose
Source7:        docker-buildx
Source8:        docker-wrapper
Conflicts:      asagiri-lite
%if 0%{?with_window}
Source9:        asagiri-window
Source10:       asagiri.desktop
Source11:       asagiri-app-icon.svg
Source12:       asagiri-app-icon-16.png
Source13:       asagiri-app-icon-32.png
Source14:       asagiri-app-icon-64.png
Source15:       asagiri-app-icon-128.png
Source16:       asagiri-app-icon-256.png
Source17:       asagiri-app-icon-512.png
Source19:       dev.asagiri.Asagiri.metainfo.xml
Requires:       gtk3
Requires:       webkit2gtk4.1
# The window's setup checklist asks for the password through pkexec.
Recommends:     polkit
%endif
%else
Conflicts:      asagiri
%endif
BuildArch:      %{asagiri_arch}
# Static binaries: nothing to find by scanning them.
AutoReq:        no
Requires(pre):  shadow-utils
%if 0%{?with_cli}
Requires(post): %{_sbindir}/update-alternatives
Requires(preun): %{_sbindir}/update-alternatives
Requires(posttrans): %{_sbindir}/update-alternatives
%endif

%description
Asagiri runs containers on this machine's kernel and speaks the Docker API on
/run/asagiri/docker.sock for users in the asagiri group (and at
/var/run/docker.sock while no Docker engine has it). No VM, no daemon other
than asagiri itself. Members of the asagiri group can run containers as root on
this machine; treat it like the docker group.
%if 0%{?with_cli}
This package brings the docker CLI, Compose and buildx with it, as Asagiri.app
does on the Mac: `docker run` works right after installing. A docker CLI from
another package keeps precedence on PATH.
%if 0%{?with_window}
Asagiri's window is in the app menu, or: asagiri ui
%endif
%else
Engine only: point any docker CLI at it with
  docker context create asagiri --docker host=unix:///run/asagiri/docker.sock
%endif

%prep

%build

%install
install -D -m 755 %{SOURCE0} %{buildroot}%{_bindir}/asagiri
install -D -m 755 %{SOURCE1} %{buildroot}%{_bindir}/asagiri-init
install -D -m 644 %{SOURCE2} %{buildroot}%{_unitdir}/asagiri.service
install -D -m 644 %{SOURCE3} %{buildroot}%{_licensedir}/%{name}/LICENSE
install -D -m 644 %{SOURCE4} %{buildroot}%{_licensedir}/%{name}/NOTICE
install -D -m 644 %{SOURCE18} %{buildroot}%{_datadir}/polkit-1/actions/dev.asagiri.host-access.policy
install -D -m 644 %{SOURCE20} %{buildroot}%{_prefix}/lib/NetworkManager/conf.d/asagiri.conf
%if 0%{?with_cli}
install -D -m 755 %{SOURCE5} %{buildroot}/usr/libexec/asagiri/bin/docker
install -D -m 755 %{SOURCE8} %{buildroot}/usr/libexec/asagiri/docker
install -D -m 755 %{SOURCE6} %{buildroot}/usr/libexec/asagiri/cli-plugins/docker-compose
install -D -m 755 %{SOURCE7} %{buildroot}/usr/libexec/asagiri/cli-plugins/docker-buildx
%endif
%if 0%{?with_window}
install -D -m 755 %{SOURCE9} %{buildroot}/usr/lib/asagiri/asagiri-window
install -D -m 644 %{SOURCE10} %{buildroot}%{_datadir}/applications/asagiri.desktop
install -D -m 644 %{SOURCE19} %{buildroot}%{_datadir}/metainfo/dev.asagiri.Asagiri.metainfo.xml
install -D -m 644 %{SOURCE11} %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/asagiri.svg
for s in 16 32 64 128 256 512; do
  install -D -m 644 %{_sourcedir}/asagiri-app-icon-$s.png %{buildroot}%{_datadir}/icons/hicolor/${s}x${s}/apps/asagiri.png
done
%endif

%pre
getent group asagiri >/dev/null || groupadd --system asagiri
exit 0

%post
# NetworkManager reads conf.d when it starts or is told to: without this,
# asagiri.conf there would wait for the next boot. Nothing to do without it.
nmcli general reload conf >/dev/null 2>&1 || :
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || :
  # Not started or enabled: it starts when someone sets Asagiri up (the window's
  # Allow access, or `asagiri host-access`). An upgrade ($1 >= 2) leaves a
  # running engine alone: it keeps the old binary until it restarts, which
  # stops its containers.
  if [ "$1" -ge 2 ] && systemctl is-active -q asagiri.service; then
    echo "Asagiri is upgraded. The engine keeps running the old version until it restarts:"
    echo "  sudo systemctl restart asagiri   (running containers stop; those with a restart policy start again)"
    echo "or open Asagiri and choose Restart engine. The next boot starts the new one either way."
  fi
fi
# A `asagiri` before /usr/bin on PATH runs instead of this package's.
for d in /usr/local/sbin /usr/local/bin; do
  if [ -e "$d/asagiri" ] && [ "$(readlink -f "$d/asagiri")" != /usr/bin/asagiri ]; then
    echo "Warning: $d/asagiri comes before /usr/bin/asagiri on PATH, so it runs instead of this Asagiri."
    echo "  If it's an older Asagiri, remove it:  sudo rm $d/asagiri"
  fi
done
%if 0%{?with_cli}
# Only a /usr/bin/docker that is absent or already an alternatives link; a
# real file, or a symlink another package or the administrator put there, is
# left alone: Asagiri's CLI must not take over someone else's docker.
free=1
if [ -e /usr/bin/docker ] || [ -L /usr/bin/docker ]; then
  case "$(readlink /usr/bin/docker 2>/dev/null)" in /etc/alternatives/*) ;; *) free=0 ;; esac
fi
if [ "$free" = 1 ]; then
  %{_sbindir}/update-alternatives --install /usr/bin/docker docker /usr/libexec/asagiri/docker 10 || :
  echo "Asagiri is installed with its own docker CLI (docker, docker compose, docker buildx)."
else
  echo "Asagiri is installed; /usr/bin/docker belongs to something else and stays. Asagiri's CLI: /usr/libexec/asagiri/docker"
fi
%else
echo "Asagiri is installed (engine only). Point a docker CLI at it:"
echo "  docker context create asagiri --docker host=unix:///run/asagiri/docker.sock && docker context use asagiri"
%endif
echo "To use it without sudo:  sudo asagiri host-access \$USER   (or open Asagiri, which sets it up)"
echo "Members of the asagiri group can run containers as root on this machine; treat it like the docker group."
echo "While no Docker engine has /var/run/docker.sock, it leads to Asagiri too (for Testcontainers, docker-py and other tools that ignore docker contexts)."
exit 0

%preun
if [ "$1" -eq 0 ]; then
  if [ -d /run/systemd/system ]; then
    systemctl disable --now asagiri.service >/dev/null 2>&1 || :
  fi
  # Everything else it runs: a window or asagiri command still open.
  for sig in TERM KILL; do
    pkill -$sig -f '^/usr/lib/asagiri/asagiri-window( |$)' >/dev/null 2>&1 || :
    pkill -$sig -f '^(/usr)?/bin/asagiri( |$)' >/dev/null 2>&1 || :
    [ "$sig" = KILL ] || sleep 1
  done
%if 0%{?with_cli}
  %{_sbindir}/update-alternatives --remove docker /usr/libexec/asagiri/docker || :
  for p in docker-compose docker-buildx; do
    %{_sbindir}/update-alternatives --remove $p /usr/libexec/asagiri/cli-plugins/$p || :
  done
%endif
fi
exit 0

%if 0%{?with_cli}
%posttrans
# Compose and buildx into /usr/lib/docker/cli-plugins where the slot is free
# and Docker's own copy isn't in another of the CLI's plugin folders; else
# Asagiri's link goes and Docker's copy is used. In %posttrans because an
# upgrade from a Asagiri that put real files there erases them only after %post;
# again whenever another package changes Docker's plugin folders.
mkdir -p /usr/lib/docker/cli-plugins
for p in docker-compose docker-buildx; do
  l=/usr/lib/docker/cli-plugins/$p
  theirs=
  for d in /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins /usr/libexec/docker/cli-plugins; do
    [ -e $d/$p ] && theirs=1
  done
  if [ -z "$theirs" ] && { [ ! -e $l ] && [ ! -L $l ] || readlink $l 2>/dev/null | grep -q '^/etc/alternatives/'; }; then
    %{_sbindir}/update-alternatives --install $l $p /usr/libexec/asagiri/cli-plugins/$p 10 || :
  elif %{_sbindir}/update-alternatives --display $p 2>/dev/null | grep -q "^/usr/libexec/asagiri/cli-plugins/$p "; then
    %{_sbindir}/update-alternatives --remove $p /usr/libexec/asagiri/cli-plugins/$p || :
  fi
done
exit 0

%filetriggerin -- /usr/lib/docker/cli-plugins /usr/libexec/docker/cli-plugins /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins
mkdir -p /usr/lib/docker/cli-plugins
for p in docker-compose docker-buildx; do
  l=/usr/lib/docker/cli-plugins/$p
  theirs=
  for d in /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins /usr/libexec/docker/cli-plugins; do
    [ -e $d/$p ] && theirs=1
  done
  if [ -z "$theirs" ] && { [ ! -e $l ] && [ ! -L $l ] || readlink $l 2>/dev/null | grep -q '^/etc/alternatives/'; }; then
    %{_sbindir}/update-alternatives --install $l $p /usr/libexec/asagiri/cli-plugins/$p 10 || :
  elif %{_sbindir}/update-alternatives --display $p 2>/dev/null | grep -q "^/usr/libexec/asagiri/cli-plugins/$p "; then
    %{_sbindir}/update-alternatives --remove $p /usr/libexec/asagiri/cli-plugins/$p || :
  fi
done
exit 0

%filetriggerpostun -- /usr/lib/docker/cli-plugins /usr/libexec/docker/cli-plugins /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins
mkdir -p /usr/lib/docker/cli-plugins
for p in docker-compose docker-buildx; do
  l=/usr/lib/docker/cli-plugins/$p
  theirs=
  for d in /usr/local/lib/docker/cli-plugins /usr/local/libexec/docker/cli-plugins /usr/libexec/docker/cli-plugins; do
    [ -e $d/$p ] && theirs=1
  done
  if [ -z "$theirs" ] && { [ ! -e $l ] && [ ! -L $l ] || readlink $l 2>/dev/null | grep -q '^/etc/alternatives/'; }; then
    %{_sbindir}/update-alternatives --install $l $p /usr/libexec/asagiri/cli-plugins/$p 10 || :
  elif %{_sbindir}/update-alternatives --display $p 2>/dev/null | grep -q "^/usr/libexec/asagiri/cli-plugins/$p "; then
    %{_sbindir}/update-alternatives --remove $p /usr/libexec/asagiri/cli-plugins/$p || :
  fi
done
exit 0
%endif

%postun
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload >/dev/null 2>&1 || :
fi
# On removal, NetworkManager forgets asagiri.conf now rather than at the next boot.
if [ "$1" -eq 0 ]; then
  nmcli general reload conf >/dev/null 2>&1 || :
fi
if [ "$1" -eq 0 ]; then
  echo "Asagiri is removed; its images, containers and volumes stay in /var/lib/asagiri"
fi
exit 0

%files
%license %{_licensedir}/%{name}/LICENSE
%license %{_licensedir}/%{name}/NOTICE
%{_bindir}/asagiri
%{_bindir}/asagiri-init
%{_unitdir}/asagiri.service
%{_datadir}/polkit-1/actions/dev.asagiri.host-access.policy
%{_prefix}/lib/NetworkManager/conf.d/asagiri.conf
%if 0%{?with_cli}
%dir /usr/libexec/asagiri
%dir /usr/libexec/asagiri/bin
/usr/libexec/asagiri/bin/docker
/usr/libexec/asagiri/docker
%dir /usr/libexec/asagiri/cli-plugins
/usr/libexec/asagiri/cli-plugins/docker-compose
/usr/libexec/asagiri/cli-plugins/docker-buildx
%endif
%if 0%{?with_window}
%dir /usr/lib/asagiri
/usr/lib/asagiri/asagiri-window
%{_datadir}/applications/asagiri.desktop
%{_datadir}/icons/hicolor/*/apps/asagiri.*
%{_datadir}/metainfo/dev.asagiri.Asagiri.metainfo.xml
%endif

%changelog
* Sun Oct 25 2026 Asagiri <AndreKurait@gmail.com> - %{asagiri_version}-1
- Packaged from the static Linux build
