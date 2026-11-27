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
echo 'PAUSING both source workloads'
docker pause "${containers[@]}" >/dev/null
for id in "${containers[@]}"; do "$asagiri" checkpoint create --external-network "$id" stream; done
"$asagiri" checkpoint fence stream "${containers[@]}"
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
