#!/usr/bin/env bash
# See packaging/README.md for the two-engine handoff and the single-engine rollback test.
set -euo pipefail
umask 077
mode=${1:?expected capture, rollback, verify or verify-volumes}
if [[ $mode == verify-volumes ]]; then
    if [[ $# != 3 || $2 == "$3" ]]; then
        echo 'supply the two distinct destination volume names after reboot' >&2
        exit 2
    fi
    shift
    for volume do
        [[ $volume =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]*$ ]]
        test "$(docker volume inspect --format '{{.Name}}' "$volume")" = "$volume"
    done
    for volume do
        docker run --rm --pull=never --network none -v "$volume:/data:ro" alpine:3.21 \
            sh -ec 'printf %s ABCD | cmp - /data/stream'
    done
    echo 'PASS both destination volumes retained ABCD'
    exit 0
fi
work=${2:?supply a new private directory on source, or its expected directory on destination}
asagiri=${ASAGIRI_BIN:-asagiri}

request() {
    printf '%s' "$1" >&3
    IFS= read -r -t 10 reply <&3
}

check_writes() {
    local front back
    read -r front back <"$expected/nonces"
    request C
    test "$reply" = "$front 3 $back 3"
    echo 'PASS first request after handoff or rollback'
    request D
    test "$reply" = "$front 4 $back 4"
    request '?'
    test "$reply" = "$(cat "$expected/connection")"
    echo 'PASS both original workloads, stream endpoints and DNS address'
}

case "$mode" in
verify)
    expected=$work
    read -r port <"$expected/port"
    [[ $port =~ ^[1-9][0-9]{0,4}$ ]] && (( port <= 65535 )) || exit 1
    exec 3<>"/dev/tcp/127.0.0.1/$port"
    check_writes
    exit 0
    ;;
capture|rollback) ;;
*) echo "unknown mode $mode" >&2; exit 2 ;;
esac

probe=${3:?supply the static Linux stream_probe binary for the engine architecture}
destination=${4:-}
test -x "$probe"
mkdir -m 700 "$work"
expected=$work/expected
mkdir "$expected"
network="stream-$$"
docker network create --subnet 10.218.47.0/24 "$network" >/dev/null
volumes=()
containers=()
for alias in backend frontend; do
    if [[ $alias == backend ]]; then
        role=server; ip=10.218.47.9; listen=42A5; publish=()
    else
        role=backend; ip=10.218.47.10; listen=42A4; publish=(-p 127.0.0.1::17060)
    fi
    volume=$(docker volume create)
    # macOS ships Bash 3, which treats an empty array as unset under nounset.
    id=$(docker create --pull=never --network "$network" --ip "$ip" --network-alias "$alias" \
        ${publish[@]+"${publish[@]}"} -v "$volume:/data" alpine:3.21 /tmp/stream-probe "$role")
    containers+=("$id"); volumes+=("$volume")
    printf '%s %s %s\n' "$alias" "$id" "$volume" >>"$work/members"
    docker cp "$probe" "$id:/tmp/stream-probe"
    docker start "$id" >/dev/null
    # An extra TCP readiness connection would consume the backend's sole accept().
    docker exec "$id" sh -ec 'i=0; until grep -q ":$1 .* 0A " /proc/net/tcp; do i=$((i+1)); test "$i" -lt 100; sleep .1; done' sh "$listen"
done
binding=$(docker port "${containers[1]}" 17060/tcp)
[[ $binding =~ ^127\.0\.0\.1:([1-9][0-9]*)$ ]]
port=${BASH_REMATCH[1]}
printf '%s\n' "$port" >"$expected/port"
exec 3<>"/dev/tcp/127.0.0.1/$port"
request A
read -r front count_front back count_back <<<"$reply"
uuid='^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
[[ $front =~ $uuid && $back =~ $uuid && $front != "$back" && $count_front == 1 && $count_back == 1 ]]
printf '%s %s\n' "$front" "$back" >"$expected/nonces"
request B
test "$reply" = "$front 2 $back 2"
request '?'
[[ $reply =~ ^10\.218\.47\.10:[1-9][0-9]*\ 10\.218\.47\.9:17061\ 10\.218\.47\.9:17061$ ]]
printf '%s\n' "$reply" >"$expected/connection"
if [[ $mode == rollback ]]; then
    "$asagiri" checkpoint platform >"$work/local-platform.json"
    destination=${destination:-"$work/local-platform.json"}
    sed -E 's/("guestArchitecture"[[:space:]]*:[[:space:]]*")[^"]+/\1different/' "$work/local-platform.json" >"$work/incompatible.json"
    if "$asagiri" checkpoint capture-group --destination "$work/incompatible.json" stream "${containers[@]}" >"$work/error" 2>&1; then exit 1; fi
    grep -q 'incompatible guest architectures' "$work/error"
    for id in "${containers[@]}"; do
        test "$(docker inspect -f '{{.State.Paused}}' "$id")" = false
        test -z "$("$asagiri" checkpoint ls "$id")"
    done
    echo 'PASS incompatible destination rejected before either workload was paused'
    # The first member has an earlier capture in this pause. Keep its snapshot and pause on failure.
    ordered=($(printf '%s\n' "${containers[@]}" | LC_ALL=C sort))
    first=${ordered[0]}; second=${ordered[1]}
    docker pause "$first" >/dev/null
    "$asagiri" checkpoint create --external-network "$first" earlier
    if "$asagiri" checkpoint capture-group earlier "$second" "$first" >"$work/error" 2>&1; then exit 1; fi
    grep -q 'already exists' "$work/error"
    if "$asagiri" checkpoint capture-group stream "$first" "${first:0:12}" >"$work/error" 2>&1; then exit 1; fi
    grep -q 'duplicate source container' "$work/error"
    # Keep an exec active in the second member through capture of the first.
    docker exec "$second" sh -c 'echo ready >/tmp/exec-ready; while test ! -f /tmp/exec-done; do sleep .1; done' &
    exec_pid=$!
    docker exec "$second" sh -ec 'i=0; until test -f /tmp/exec-ready; do i=$((i+1)); test "$i" -lt 100; sleep .1; done'
    if "$asagiri" checkpoint capture-group --external-network stream "$second" "$first" >"$work/error" 2>&1; then exit 1; fi
    grep -q 'finish the active container exec' "$work/error"
    test "$(docker inspect -f '{{.State.Paused}}' "$first")" = true
    test "$(docker inspect -f '{{.State.Paused}}' "$second")" = false
    test "$("$asagiri" checkpoint ls "$first")" = earlier
    test -z "$("$asagiri" checkpoint ls "$second")"
    "$asagiri" checkpoint fence earlier "$first"
    "$asagiri" checkpoint release earlier "$first"
    docker unpause "$first" >/dev/null
    docker exec "$second" touch /tmp/exec-done
    wait "$exec_pid"
    request '?'
    test "$reply" = "$(cat "$expected/connection")"
    echo 'PASS failed second capture preserved the earlier capture, pause state and live connection'
fi
echo 'PAUSING both source workloads'
if [[ -n $destination ]]; then
    "$asagiri" checkpoint capture-group --destination "$destination" --external-network stream "${containers[@]}" >"$work/preflight.json"
    for id in "${containers[@]}"; do grep -Fq "$id" "$work/preflight.json"; done
else
    "$asagiri" checkpoint capture-group --external-network stream "${containers[@]}"
fi
for id in "${containers[@]}"; do
    test "$(docker inspect -f '{{.State.Paused}}' "$id")" = true
    if docker unpause "$id" >"$work/error" 2>&1; then exit 1; fi
    grep -q 'fenced for checkpoint stream' "$work/error"
done
for i in 0 1; do "$asagiri" checkpoint export "${containers[$i]}" stream "$work/$i.tar"; done
echo 'FENCED both source workloads and exported both archives'

if [[ $mode == capture ]]; then
    # The test operator now shuts down the disposable source, with both workloads fenced.
    # Bash returns 1 for EOF and greater than 128 on timeout. No reply is expected.
    rc=0; IFS= read -r -t 120 reply <&3 || rc=$?
    [[ $rc == 1 && -z $reply ]]
    echo 'PASS original external connection ended at source shutdown'
else
    "$asagiri" checkpoint release stream "${containers[@]}"
    docker unpause "${containers[@]}" >/dev/null
    check_writes
    for id in "${containers[@]}"; do test "$(docker exec "$id" cat /data/stream)" = ABCD; done
    docker rm -f "${containers[@]}" >/dev/null
    docker volume rm "${volumes[@]}" >/dev/null
    docker network rm "$network" >/dev/null
    isolated=$(docker run --pull=never --network none -d alpine:3.21 sleep 60)
    if "$asagiri" checkpoint create --external-network "$isolated" invalid >"$work/error" 2>&1; then exit 1; fi
    grep -q 'own engine-managed bridge' "$work/error"
    test -z "$("$asagiri" checkpoint ls "$isolated")"
    "$asagiri" checkpoint create "$isolated" ordinary
    test "$("$asagiri" checkpoint ls "$isolated")" = ordinary
    docker rm -f "$isolated" >/dev/null
    echo 'PASS ordinary capture on an isolated network and rejection of external-network capture'
    echo 'PASS live rollback retained the original external connection and both open volume files'
fi
