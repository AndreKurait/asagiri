#!/bin/sh
# Runs Asagiri's speed check (scripts/perf.py) for two Linux builds: main's and
# a change's. Each engine runs in its own privileged container on this
# machine's Docker, with its socket in WORK, and the check talks to both.
#
#   scripts/perf-run.sh BASE_BIN HEAD_BIN WORK [RUNS]
#
# BASE_BIN and HEAD_BIN hold the engine and its *-init companion. Each side
# can have a different product name; the limits still come from main.
# Needs docker, python3 and strace, and root for strace and the sockets
# (it uses sudo when not root). Writes WORK/report.md; exits 1 when the
# change is past a limit.
set -eu
BASE_BIN=$(cd "$1" && pwd); HEAD_BIN=$(cd "$2" && pwd); WORK=$3; RUNS=${4:-400}
HERE=$(cd "$(dirname "$0")" && pwd)
SUDO=; [ "$(id -u)" = 0 ] || SUDO=sudo
mkdir -p "$WORK"; WORK=$(cd "$WORK" && pwd)

stop() {
  docker rm -f asagiri-perf-base asagiri-perf-head >/dev/null 2>&1 || true
  docker volume rm asagiri-perf-base-lib asagiri-perf-base-store asagiri-perf-head-lib asagiri-perf-head-store >/dev/null 2>&1 || true
}
trap stop EXIT
stop
for e in base head; do
  bin=$BASE_BIN; [ $e = head ] && bin=$HEAD_BIN
  set -- "$bin/"*-init
  [ "$#" -eq 1 ] && [ -x "$1" ] || { echo "expected one executable *-init in $bin"; exit 2; }
  engine=$(basename "$1" -init)
  [ -x "$bin/$engine" ] || { echo "missing engine $bin/$engine"; exit 2; }
  socket_var=$(printf '%s_SOCKET' "$engine" | tr '[:lower:]' '[:upper:]')
  $SUDO rm -rf "$WORK/$e-run"; mkdir -p "$WORK/$e-run"
  # The socket is named, so both engines listen where the check looks even
  # when the two builds differ in where they would put it.
  docker run -d --name asagiri-perf-$e --privileged --tmpfs /run \
    -v "asagiri-perf-$e-lib:/var/lib/$engine" -v "asagiri-perf-$e-store:/root/.local/share/$engine" \
    -v "$bin:/opt/engine:ro" -v "$WORK/$e-run:/bench" \
    -e "$socket_var=/bench/docker.sock" \
    "${ASAGIRI_CI_PERF_IMAGE:-alpine:3.21}" "/opt/engine/$engine" daemon >/dev/null
done
pid() { docker inspect -f '{{.State.Pid}}' "asagiri-perf-$1"; }
for e in base head; do
  i=0; while [ ! -S "$WORK/$e-run/docker.sock" ]; do
    i=$((i + 1)); [ $i -gt 120 ] && { docker logs asagiri-perf-$e 2>&1 | tail -20; echo "the $e engine made no socket"; exit 2; }
    sleep 0.5
  done
done
python3 "$HERE/perf.py" sizes --base "$BASE_BIN" --head "$HEAD_BIN" --out "$WORK/sizes.json"
$SUDO python3 "$HERE/perf.py" bench --runs "$RUNS" --out "$WORK/results.json" \
  --base "$WORK/base-run/docker.sock:$(pid base)" --head "$WORK/head-run/docker.sock:$(pid head)"
status=0
python3 "$HERE/perf.py" report "$WORK/results.json" "$WORK/sizes.json" > "$WORK/report.md" || status=$?
cat "$WORK/report.md"
exit $status
