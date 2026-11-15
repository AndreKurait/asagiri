#!/bin/sh
# Asagiri Linux's asagiri-start (contrib/wsl/asagiri-linux), in the Alpine it runs
# on, with a small compiled stand-in that, as the real one, makes its
# socket and, on SIGTERM, removes it first and exits a few seconds later
# (crates/asagiri/src/daemon/mod.rs, stopping containers).
# - four started at once start one daemon, all exit 0, and the daemon does
#   not keep their lock;
# - one whose daemon never makes its socket says so (exit 1); one whose
#   daemon exits at once is exit 2;
# - started while the daemon is stopping (Restart engine's kill, then a
#   start), it waits for that one to go and starts another;
# - --restart stops the daemon, waits for it, and starts a new one, also
#   when its shutdown takes 20 s (containers with a long --stop-timeout).
# The checks' own pgrep spell it "[a]sagiri", so as not to match the script
# they run in. Usage: tests/asagiri-start.sh DAEMON_FIXTURE PINNED_ALPINE_IMAGE
set -u
[ "$#" -eq 2 ] || { echo "usage: asagiri-start.sh DAEMON_FIXTURE PINNED_ALPINE_IMAGE" >&2; exit 2; }
helper=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
[ -f "$helper" ] || exit 2
image=$2
case "$image" in *@sha256:*|sha256:*) sum=${image##*sha256:} ;; *) echo "Use a digest-pinned Alpine image" >&2; exit 2 ;; esac
[ "${#sum}" -eq 64 ] || exit 2
case "$sum" in *[!a-f0-9]*) exit 2 ;; esac
D=${D:-docker}
DIR=$(cd "$(dirname "$0")/.." && pwd)
owner="asagiri-wsl-start-$$-$(date +%s)"
printf 'fixture: %s\n' "$owner"
cleanup() {
  label=$("$D" inspect --format '{{ index .Config.Labels "dev.asagiri.wsl-start-test" }}' "$owner" 2>/dev/null) || return 0
  [ "$label" = "$owner" ] && "$D" rm -f "$owner" >/dev/null 2>&1
}
finish() {
  result=$?
  trap - EXIT
  cleanup || result=1
  if output=$("$D" inspect "$owner" 2>&1); then
    echo "FAIL: fixture still exists: $owner" >&2
    result=1
  elif ! printf '%s\n' "$output" | grep -Eqi 'no such (object|container)'; then
    printf 'FAIL: cannot verify fixture cleanup: %s\n' "$output" >&2
    result=1
  else
    printf 'cleanup: verified absent: %s\n' "$owner"
  fi
  exit "$result"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# The script is for the container's sh: its $ are expanded there.
# shellcheck disable=SC2016
"$D" run --rm --pull never --network none --name "$owner" \
  --label "dev.asagiri.wsl-start-test=$owner" -e ASAGIRI_TEST_WSL_START=1 \
  -v "$helper:/fixture/asagiri-daemon:ro" \
  -v "$DIR/contrib/wsl/asagiri-linux/asagiri-start:/usr/local/bin/asagiri-start:ro" "$image" sh -c '
  cp /fixture/asagiri-daemon /usr/local/bin/asagiri
  chmod 755 /usr/local/bin/asagiri
  S=/var/lib/asagiri-daemon/.asagiri/run/docker.sock
  daemons() { pgrep -f "/usr/local/bin/[a]sagiri daemon"; }
  fail=0
  echo up >/tmp/mode
  for n in 1 2 3 4; do { sh /usr/local/bin/asagiri-start >/tmp/out$n 2>&1; echo $? >/tmp/code$n; } & done
  wait
  [ "$(wc -l </tmp/launches)" -eq 1 ] || { echo "FAIL: $(wc -l </tmp/launches) daemons started"; fail=1; }
  [ "$(cat /tmp/code* | tr -d "\n")" = 0000 ] || { echo "FAIL: four at once: exit codes $(cat /tmp/code* | tr "\n" " ")"; cat /tmp/out*; fail=1; }
  pid=$(daemons | head -1)
  if [ -z "$pid" ] || [ ! -S $S ]; then
    echo "FAIL: no daemon serves its socket"; fail=1
  elif ls -l "/proc/$pid/fd" | grep -q asagiri-start.lock; then
    echo "FAIL: the daemon keeps the lock"; fail=1
  fi
  # Restart engine as it was: the daemon killed, then a start while it
  # stops (its socket gone, its process not yet).
  pkill -f "/usr/local/bin/[a]sagiri daemon"; sleep 1
  sh /usr/local/bin/asagiri-start 2>/tmp/err; code=$?
  new=$(daemons)
  if [ $code != 0 ] || [ -z "$new" ] || [ "$new" = "$pid" ] || [ ! -S $S ]; then
    echo "FAIL: a start while the daemon stops: exit $code, daemon ${new:-none} (was $pid), $(cat /tmp/err)"; fail=1
  fi
  # --restart: the old one stopped and waited for, a new one up.
  pid=$new
  sh /usr/local/bin/asagiri-start --restart 2>/tmp/err; code=$?
  new=$(daemons)
  if [ $code != 0 ] || [ -z "$new" ] || [ "$new" = "$pid" ] || [ ! -S $S ]; then
    echo "FAIL: --restart: exit $code, daemon ${new:-none} (was $pid), $(cat /tmp/err)"; fail=1
  fi
  # --restart of a daemon whose shutdown takes longer than 15 s.
  pid=$new
  echo 20 >/tmp/stopwait
  sh /usr/local/bin/asagiri-start --restart 2>/tmp/err; code=$?
  rm -f /tmp/stopwait
  new=$(daemons)
  if [ $code != 0 ] || [ -z "$new" ] || [ "$new" = "$pid" ] || [ ! -S $S ]; then
    echo "FAIL: --restart of a slow stop: exit $code, daemon ${new:-none} (was $pid), $(cat /tmp/err)"; fail=1
  fi
  pkill -9 -f "/usr/local/bin/[a]sagiri daemon"; rm -f $S; sleep 0.2
  # A daemon that never makes its socket: the start says so, exit 1.
  echo nosocket >/tmp/mode
  sh /usr/local/bin/asagiri-start 2>/tmp/err; code=$?
  [ $code = 1 ] && grep -q "did not create its socket" /tmp/err || { echo "FAIL: no socket: exit $code, $(cat /tmp/err)"; fail=1; }
  pkill -9 -f "/usr/local/bin/[a]sagiri daemon"; sleep 0.2
  # A daemon that exits as it starts: exit 2, at once.
  echo exits >/tmp/mode
  sh /usr/local/bin/asagiri-start 2>/tmp/err; code=$?
  [ $code = 2 ] && grep -q "exited as it started" /tmp/err || { echo "FAIL: a daemon that exits: exit $code, $(cat /tmp/err)"; fail=1; }
  [ $fail = 0 ] && echo "ok: one daemon for four starts; a start while it stops, and --restart (a slow stop too), start a new one; no socket is exit 1, one that exits is exit 2"
  exit $fail'
