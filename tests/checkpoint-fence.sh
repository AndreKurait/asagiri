#!/bin/sh
# A disposable checkpoint-capable engine, with Alpine 3.21 loaded. Run "rebooted"
# on the same store after restarting the daemon to verify the durable fence.
set -eu
umask 077
asagiri=${ASAGIRI_BIN:-asagiri}
work=${2:?supply a private test directory}
reject() {
    if "$@" >"$work/error" 2>&1; then echo "unexpected success: $*" >&2; exit 1; fi
}
state() { docker inspect --format '{{.State.Running}} {{.State.Paused}} {{.State.Pid}}' "$1"; }
fence() {
    response=$(curl -fsS --unix-socket "${ASAGIRI_SOCKET:?}" "http://localhost/containers/$1/json") || return 1
    printf '%s\n' "$response" | jq -er '.State | if has("AsagiriCheckpointFence") then (.AsagiriCheckpointFence // "") else error("missing fence field") end'
}
query() { docker exec "$1" timeout 10 sh -ec 'printf "%s\n" "$1" >/tmp/query; cat /tmp/reply' sh "$2"; }
case ${1:-} in
prepare)
    test ! -f "$work/members"
    for n in one two; do
        volume=$(docker volume create)
        id=$(docker run --pull=never --network none --restart always -d -v "$volume:/data" alpine:3.21 sh -ec '
            trap "echo thawed >>/data/term; exit 0" TERM
            read -r nonce </proc/sys/kernel/random/uuid
            exec 3>>/data/bytes
            mkfifo /tmp/query /tmp/reply
            while read -r value </tmp/query; do printf %s "$value" >&3; printf "%s\n" "$nonce" >/tmp/reply; done')
        printf '%s %s\n' "$id" "$volume" >>"$work/members"
        for _ in 1 2 3 4 5; do if docker exec "$id" test -p /tmp/reply; then break; fi; sleep 1; done
        query "$id" A >"$work/$n.nonce"
        test "$(wc -c <"$work/$n.nonce")" -eq 37
        state "$id" >"$work/$n.state"
        docker pause "$id" >/dev/null
    done
    a=$(awk 'NR==1 {print $1}' "$work/members"); b=$(awk 'NR==2 {print $1}' "$work/members")
    docker checkpoint create --leave-running "$a" group
    reject "$asagiri" checkpoint fence group "$a" "$b"
    test -z "$(fence "$a")" && test -z "$(fence "$b")"
    docker checkpoint create --leave-running "$b" group
    reject "$asagiri" checkpoint fence group "$a" "$a"
    docker unpause "$b" >/dev/null
    docker pause "$b" >/dev/null
    reject "$asagiri" checkpoint fence group "$a" "$b"
    test -z "$(fence "$a")" && test -z "$(fence "$b")"
    if [ -n "${FENCE_TEST_RECORDS:-}" ]; then
        # A directory at the temporary-file path injects a save failure in this disposable store.
        mkdir "$FENCE_TEST_RECORDS.tmp"
        reject "$asagiri" checkpoint capture-group fresh "$a" "$b"
        grep -q 'could not persist checkpoint fence' "$work/error"
        rmdir "$FENCE_TEST_RECORDS.tmp"
        for id in "$a" "$b"; do
            reject docker unpause "$id"
            test "$(fence "$id")" = fresh
            "$asagiri" checkpoint ls "$id" | grep -qx fresh
        done
    else
        "$asagiri" checkpoint capture-group fresh "$a" "$b"
    fi
    "$asagiri" checkpoint fence fresh "$a" "$b"
    "$asagiri" checkpoint fence fresh "$b" "$a"
    if [ -n "${FENCE_TEST_RECORDS:-}" ]; then
        mkdir "$FENCE_TEST_RECORDS.tmp"
        reject "$asagiri" checkpoint release fresh "$a" "$b"
        grep -q 'could not persist checkpoint fence' "$work/error"
        rmdir "$FENCE_TEST_RECORDS.tmp"
        reject docker unpause "$a"; reject docker unpause "$b"
    fi
    reject "$asagiri" checkpoint release wrong "$a" "$b"
    for id in "$a" "$b"; do
        test "$(fence "$id")" = fresh
        reject docker unpause "$id"; reject docker restart -t 0 "$id"; reject docker kill --signal CONT "$id"
        reject docker stop -t 0 "$id"; reject docker rm -f "$id"
        "$asagiri" checkpoint export "$id" fresh "$work/$id.tar"
        test -s "$work/$id.tar"
    done
    "$asagiri" checkpoint release fresh "$a" "$b"
    for n in one two; do
        if [ "$n" = one ]; then id=$a; else id=$b; fi
        test -z "$(fence "$id")"
        test "$(state "$id")" = "$(sed 's/true false/true true/' "$work/$n.state")"
        docker unpause "$id" >/dev/null
        test "$(query "$id" B)" = "$(cat "$work/$n.nonce")"
        test "$(docker exec "$id" cat /data/bytes)" = AB
        docker pause "$id" >/dev/null
        docker checkpoint create --leave-running "$id" committed
    done
    "$asagiri" checkpoint fence committed "$a" "$b"
    echo 'PASS two-workload rollback preserved both PIDs, RAM values and descriptor writes'
    ;;
rebooted)
    while read -r id volume; do
        test "$(fence "$id")" = committed
        test "$(state "$id")" = 'false false 0'
        reject docker start "$id"
        docker run --rm --pull=never --network none -v "$volume:/data:ro" alpine:3.21 sh -ec 'test "$(cat /data/bytes)" = AB && test ! -e /data/term'
    done <"$work/members"
    a=$(awk 'NR==1 {print $1}' "$work/members"); b=$(awk 'NR==2 {print $1}' "$work/members")
    "$asagiri" checkpoint release committed "$a" "$b"
    while read -r id volume; do
        test -z "$(fence "$id")"; docker rm "$id" >/dev/null; docker volume rm "$volume" >/dev/null
    done <"$work/members"
    echo 'PASS daemon restart kept both restart-always sources stopped and retained both volumes'
    ;;
*) echo 'expected prepare or rebooted' >&2; exit 2;;
esac
