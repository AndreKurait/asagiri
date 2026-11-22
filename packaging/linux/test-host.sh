#!/bin/bash
# Installs each package on THIS machine (root, systemd): the .deb where
# there is dpkg (Debian, Ubuntu), else the .rpm (Fedora, RHEL), runs containers
# through the packaged engine, and removes the package again. Images,
# containers and volumes in /var/lib/asagiri are kept (remove, never purge). For
# a test machine or VM: a asagiri daemon already running here is stopped first
# (only with ASAGIRI_TEST_KILL=1, since its containers end with it) and not
# started again.
#   sudo ASAGIRI_TEST_KILL=1 packaging/linux/test-host.sh <outdir> [static docker CLI dir]
# Exit status is the number of failed checks.
set -u
out=$(cd "$1" && pwd) clidir=${2:-}
fail=0
ok() { echo "  [ok]   $1"; }
bad() { echo "  [FAIL] $1"; fail=$((fail + 1)); }
log=$(mktemp)
trap 'rm -f "$log"' EXIT
check_command() {
  local description=$1
  shift
  if "$@" >"$log" 2>&1; then
    ok "$description"
    sed -n '/^asagiri-package-upgrade /p' "$log"
  else
    bad "$description"
    sed 's/^/         /' "$log" | tail -4
  fi
}
check() { check_command "$1" eval "$2"; }

# Called by the lifecycle functions dispatched through check_command.
# shellcheck disable=SC2329
engine_ready() {
  local _attempt
  for _attempt in $(seq 30); do
    if [ "$(curl -fs --max-time 2 --unix-socket /run/asagiri/docker.sock http://asagiri/_ping)" = OK ]; then
      return 0
    fi
    sleep 1
  done
  echo "The packaged engine did not answer _ping within the deadline" >&2
  return 1
}

# Called only after this test has started the packaged engine.
# shellcheck disable=SC2329
service_task_policy() {
  local group
  [ "$(systemctl show -p Delegate --value asagiri)" = yes ] || return 1
  [ "$(systemctl show -p TasksMax --value asagiri)" = infinity ] || return 1
  group=$(systemctl show -p ControlGroup --value asagiri) || return 1
  [ -n "$group" ] && [ "$group" != / ] || return 1
  [ "$(cat "/sys/fs/cgroup$group/pids.max")" = max ]
}

# Passed as check_command's command, without eval.
# shellcheck disable=SC2329
reinstall_running_engine() { # format package
  local old new invocation current started installed
  old=$(systemctl show -p MainPID --value asagiri)
  invocation=$(systemctl show -p InvocationID --value asagiri)
  started=$(systemctl show -p ExecMainStartTimestampMonotonic --value asagiri)
  [ "$old" -gt 1 ] && [ -n "$invocation" ] && engine_ready || return 1
  # DEB and RPM upgrades deliberately keep the old engine running. Reinstalling
  # the same package exercises that policy without inventing another version.
  case "$1" in
    deb) dpkg -i "$2" || return 1 ;;
    rpm) dnf reinstall -y -q "$2" || return 1 ;;
    *) return 1 ;;
  esac
  new=$(systemctl show -p MainPID --value asagiri)
  current=$(systemctl show -p InvocationID --value asagiri)
  installed=$(stat -Lc '%d:%i' /usr/bin/asagiri) || return 1
  printf 'asagiri-package-upgrade phase=reinstall old_pid=%s new_pid=%s old_invocation=%s new_invocation=%s installed_inode=%s\n' \
    "$old" "$new" "$invocation" "$current" "$installed"
  [ "$new" = "$old" ] && [ "$current" = "$invocation" ] &&
    [ "$(systemctl show -p ExecMainStartTimestampMonotonic --value asagiri)" = "$started" ] &&
    engine_ready
}

# Passed as check_command's command, without eval.
# shellcheck disable=SC2329
activate_reinstalled_engine() { # account
  local old new invocation current running installed rc=0
  old=$(systemctl show -p MainPID --value asagiri)
  invocation=$(systemctl show -p InvocationID --value asagiri)
  [ "$old" -gt 1 ] && [ -n "$invocation" ] || return 1
  # This is the window's explicit Restart engine action, through pkexec.
  /usr/bin/asagiri host-access --restart "$1" || rc=$?
  { [ "$rc" = 0 ] || [ "$rc" = 10 ]; } && engine_ready || return 1
  new=$(systemctl show -p MainPID --value asagiri)
  current=$(systemctl show -p InvocationID --value asagiri)
  running=$(stat -Lc '%d:%i' "/proc/$new/exe") || return 1
  installed=$(stat -Lc '%d:%i' /usr/bin/asagiri) || return 1
  printf 'asagiri-package-upgrade phase=activate old_pid=%s new_pid=%s old_invocation=%s new_invocation=%s running_inode=%s installed_inode=%s\n' \
    "$old" "$new" "$invocation" "$current" "$running" "$installed"
  [ "$new" -gt 1 ] && [ "$new" != "$old" ] &&
    [ -n "$current" ] && [ "$current" != "$invocation" ] &&
    [ "$running" = "$installed" ]
}
[ -d /run/systemd/system ] || { echo "no systemd here; use test.sh"; exit 1; }
command -v pgrep >/dev/null 2>&1 || { echo "pgrep is required (install procps/procps-ng)"; exit 1; }
if pgrep -x asagiri >/dev/null; then
  if [ "${ASAGIRI_TEST_KILL:-}" = 1 ]; then pkill -x asagiri; sleep 1; else
    echo "a asagiri daemon is running on this machine; this test stops it (and every container it runs) and does not start it again. Set ASAGIRI_TEST_KILL=1 to go on." >&2
    exit 1
  fi
fi

if command -v dpkg >/dev/null; then fmt=deb; else fmt=rpm; fi
# The package file for asagiri or asagiri-lite (asagiri-[0-9]* so asagiri doesn't match asagiri-lite).
pkgfile() { if [ "$fmt" = deb ]; then ls "$out"/"${1}"_*.deb 2>/dev/null; else ls "$out"/"${1}"-[0-9]*.rpm 2>/dev/null; fi; }

host_test() { # name full(0|1)
  local name=$1 full=$2 deb cli='' slot_free=1 install remove
  deb=$(pkgfile "$name")
  # Use the host package manager to resolve dependencies on first install.
  if [ "$fmt" = deb ]; then
    install="DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends '$deb'"
    remove="dpkg -r $name"
  else
    install="dnf install -y -q '$deb'"
    remove="rpm -e $name"
  fi
  # postinst offers Asagiri's docker through update-alternatives only when
  # /usr/bin/docker is absent or already an alternatives link; one that
  # belongs to something else (Docker Desktop's symlink in WSL, a docker-ce
  # file) must be left alone, and Asagiri's CLI is then /usr/libexec/asagiri/docker.
  if [ -e /usr/bin/docker ] || [ -L /usr/bin/docker ]; then
    case "$(readlink /usr/bin/docker 2>/dev/null)" in /etc/alternatives/*) ;; *) slot_free=0 ;; esac
  fi
  # The unit links /var/run/docker.sock to Asagiri only when nothing is there.
  local sock_free=1
  { [ -e /run/docker.sock ] || [ -L /run/docker.sock ]; } && sock_free=0
  echo
  # shellcheck source=/dev/null
  echo "#### $name on $(. /etc/os-release; echo "$PRETTY_NAME")"
  check "${install%% *} install $(basename "$deb")" "$install"
  # Installing runs nothing: the engine starts when someone sets Asagiri up.
  check "installed, engine not started or enabled" "! systemctl is-enabled -q asagiri && ! systemctl is-active -q asagiri && test ! -S /run/asagiri/docker.sock"
  # The window's setup checklist: \`asagiri host-access USER\` (through pkexec)
  # starts the engine and lets an account in at once, by name on the socket's
  # access list, without it signing in again when the filesystem supports
  # ACLs. Otherwise exit 10 requests a new login with the updated groups.
  check "polkit action for the password prompt" "test -f /usr/share/polkit-1/actions/dev.asagiri.host-access.policy"
  check "NetworkManager leaves Asagiri's interfaces alone" "grep -q 'interface-name:asagiri\*;interface-name:kv\*;interface-name:kx\*' /usr/lib/NetworkManager/conf.d/asagiri.conf"
  userdel asagiri-acl-test >/dev/null 2>&1; useradd --system --no-create-home asagiri-acl-test
  userdel asagiri-acl-other >/dev/null 2>&1; useradd --system --no-create-home asagiri-acl-other
  local access_status=0
  /usr/bin/asagiri host-access asagiri-acl-test >"$log" 2>&1 || access_status=$?
  case "$access_status" in
    0|10) ok "host-access completed (status $access_status)" ;;
    *) bad "host-access failed (status $access_status)"; tail -4 "$log" ;;
  esac
  check "host-access starts the engine and adds the account to the asagiri group" "systemctl is-enabled -q asagiri && systemctl is-active -q asagiri && id -nG asagiri-acl-test | grep -qw asagiri"
  check_command "service delegates container tasks without an aggregate task ceiling" service_task_policy
  for _ in $(seq 30); do [ -S /run/asagiri/docker.sock ] && break; sleep 1; done
  check "socket /run/asagiri/docker.sock, group asagiri, mode 660" "test -S /run/asagiri/docker.sock && stat -c '%G %a' /run/asagiri/docker.sock | grep -q '^asagiri 660'"
  check "an account without host-access is refused" "! setpriv --reuid=asagiri-acl-other --regid=asagiri-acl-other --clear-groups curl -fs --unix-socket /run/asagiri/docker.sock http://asagiri/_ping"
  userdel asagiri-acl-other >/dev/null 2>&1
  if [ "$access_status" -eq 0 ]; then
    check "ACL access works without signing in again" "setpriv --reuid=asagiri-acl-test --regid=asagiri-acl-test --clear-groups curl -fs --unix-socket /run/asagiri/docker.sock http://asagiri/_ping | grep -q OK"
  elif [ "$access_status" -eq 10 ]; then
    check "without ACLs, an old session still needs its new group" "! setpriv --reuid=asagiri-acl-test --regid=asagiri-acl-test --clear-groups curl -fs --unix-socket /run/asagiri/docker.sock http://asagiri/_ping"
  fi
  check "a new session has access through its group" "setpriv --reuid=asagiri-acl-test --regid=asagiri-acl-test --init-groups curl -fs --unix-socket /run/asagiri/docker.sock http://asagiri/_ping | grep -q OK"
  check "group access survives an engine restart" "systemctl restart asagiri && for i in \$(seq 30); do [ -S /run/asagiri/docker.sock ] && break; sleep 1; done; setpriv --reuid=asagiri-acl-test --regid=asagiri-acl-test --init-groups curl -fs --unix-socket /run/asagiri/docker.sock http://asagiri/_ping | grep -q OK"
  if [ "$access_status" -eq 0 ]; then
    check "ACL access also survives the restart" "setpriv --reuid=asagiri-acl-test --regid=asagiri-acl-test --clear-groups curl -fs --unix-socket /run/asagiri/docker.sock http://asagiri/_ping | grep -q OK"
  fi
  check_command "package reinstall preserves the running engine and healthy API" reinstall_running_engine "$fmt" "$deb"
  check_command "explicit restart activates the reinstalled engine" activate_reinstalled_engine asagiri-acl-test
  userdel asagiri-acl-test >/dev/null 2>&1
  check "/run/asagiri is root 755 (RuntimeDirectory; the socket itself is 660 root:asagiri)" "stat -c '%U %a' /run/asagiri | grep -q '^root 755'"
  check "/var/lib/asagiri-daemon (StateDirectory) exists" "test -d /var/lib/asagiri-daemon"
  if [ "$sock_free" = 1 ]; then
    check "/var/run/docker.sock links to Asagiri's socket" "test \"\$(readlink /var/run/docker.sock)\" = /run/asagiri/docker.sock"
  else
    check "/var/run/docker.sock ($(readlink /var/run/docker.sock 2>/dev/null || echo a socket)) left to its owner" "test \"\$(readlink /var/run/docker.sock 2>/dev/null)\" != /run/asagiri/docker.sock"
  fi
  if [ "$full" = 1 ]; then
    if [ "$slot_free" = 1 ]; then
      check "/usr/bin/docker is Asagiri's (update-alternatives)" "readlink -f /usr/bin/docker | grep -q /usr/libexec/asagiri/docker"
      cli=docker
    else
      check "/usr/bin/docker ($(readlink /usr/bin/docker 2>/dev/null || echo a file)) left to its owner" "! readlink -f /usr/bin/docker | grep -q /usr/libexec/asagiri/docker"
      cli=/usr/libexec/asagiri/docker
    fi
    check "$cli version, no DOCKER_HOST needed" "for i in \$(seq 30); do $cli version >/dev/null 2>&1 && break; sleep 1; done; $cli version --format '{{.Server.Os}}' | grep -q linux"
    # The plugins as laid down; found through the CLI only when nothing else
    # shadows them (a docker in /usr/local/lib/docker/cli-plugins, Docker
    # Desktop's WSL integration for one, is searched first).
    check "compose and buildx plugins run (/usr/libexec/asagiri/cli-plugins)" "/usr/libexec/asagiri/cli-plugins/docker-compose version | grep -q v5 && /usr/libexec/asagiri/cli-plugins/docker-buildx version | grep -q v0.38"
    [ "$slot_free" = 1 ] && check "docker compose and buildx found through the CLI" "$cli compose version | grep -q v5 && $cli buildx version | grep -q v0.38"
  elif [ -n "$clidir" ]; then
    cli="$clidir/docker -H unix:///run/asagiri/docker.sock"
    check "docker version through the socket" "for i in \$(seq 30); do $cli version >/dev/null 2>&1 && break; sleep 1; done; $cli version --format '{{.Server.Os}}' | grep -q linux"
  fi
  if [ -n "$cli" ]; then
    if [ -n "${ASAGIRI_EXPECTED_VERSION:-}" ]; then
      check "service runs the stamped release version" "test \"\$($cli version --format '{{index (index .Server.Components 0).Details \"AsagiriVersion\"}}')\" = '$ASAGIRI_EXPECTED_VERSION'"
    fi
    check "docker run alpine" "$cli run --rm alpine cat /etc/alpine-release | grep -q '^3\.'"
    # The Linux agent creates this directory lazily, on the first container.
    check "/var/lib/asagiri is 710, not world-readable" "test \"\$(stat -c %a /var/lib/asagiri)\" = 710"
    if [ "$fail" -gt 0 ]; then
      # Why a container doesn't start: the unit's cgroup tree and the engine's log.
      echo "         cgroup tree of asagiri.service:"
      find /sys/fs/cgroup/system.slice/asagiri.service -maxdepth 2 -type d 2>&1 | sed 's/^/           /'
      echo "         asagiri-init's cgroup: $(cat "/proc/$(pgrep -x asagiri-init | head -1)/cgroup" 2>&1)"
      journalctl -u asagiri -n 25 --no-pager 2>&1 | cut -c1-220 | sed 's/^/           /'
    fi
    check "container has internet" "$cli run --rm alpine wget -qO- -T 20 http://example.com | grep -q 'Example Domain'"
    if [ "$sock_free" = 1 ] && [ -n "$clidir" ]; then
      check "docker through /var/run/docker.sock, no context" "$clidir/docker -H unix:///var/run/docker.sock version --format '{{.Server.Os}}' | grep -q linux"
    fi
    # Testcontainers' Ryuk: a container driving the engine through the socket it was given.
    # Asagiri's socket, wherever it is: /var/run/docker.sock when Asagiri linked it,
    # else /run/asagiri/docker.sock (the other socket, Docker Desktop's in WSL
    # for one, is not Asagiri). The server must say it's Asagiri.
    local ksock=/var/run/docker.sock; [ "$sock_free" = 1 ] || ksock=/run/asagiri/docker.sock
    check "container mounts Asagiri's socket and reaches Asagiri" "$cli run --rm -v $ksock:/var/run/docker.sock docker:cli docker version --format '{{.Server.Platform.Name}} {{.Server.Version}}' | grep -qi asagiri"
    check "published port answers" "$cli rm -f asagiri-pkg-web >/dev/null 2>&1; $cli run -d --name asagiri-pkg-web -p 127.0.0.1:18083:80 nginx:alpine >/dev/null && for i in \$(seq 40); do curl -fs 127.0.0.1:18083 >/dev/null 2>&1 && break; sleep 0.5; done; curl -fs 127.0.0.1:18083 | grep -q 'Welcome to nginx'; r=\$?; $cli rm -f asagiri-pkg-web >/dev/null 2>&1; [ \$r -eq 0 ]"
  else
    echo "  [skip] docker run (pass a static docker CLI dir as the 2nd argument)"
  fi
  check "$remove" "$remove"
  check "service stopped, binaries gone, data kept" "! systemctl is-active asagiri && test ! -e /usr/bin/asagiri && test -d /var/lib/asagiri"
  [ "$sock_free" = 1 ] && check "/var/run/docker.sock link gone with the service" "test ! -L /var/run/docker.sock"
  [ "$full" = 1 ] && check "docker alternative withdrawn" "test ! -e /usr/bin/docker || ! readlink -f /usr/bin/docker | grep -q libexec/asagiri"
}

host_test asagiri-lite 0
[ -n "$(pkgfile asagiri)" ] && host_test asagiri 1
echo
echo "== $fail failed"
exit $fail
