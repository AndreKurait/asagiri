#!/bin/sh
# Run against a disposable, idle Mac engine with Alpine already loaded.
set -eu
socket=${1:?provide the disposable engine socket}
image=${2:-alpine:3.21}
resources=http://localhost/asagiri/resources
original=$(curl -fsS --unix-socket "$socket" "$resources")
assert_idle=$(printf '%s' "$original" | python3 -c 'import json,sys; print(json.load(sys.stdin)["awake"])')
[ "$assert_idle" = False ] || { echo 'The test requires a sleeping VM' >&2; exit 1; }
post() { curl -fsS --unix-socket "$socket" -H 'Content-Type: application/json' -d "$1" "$resources" >/dev/null; }
sleep_vm() { curl -fsS --unix-socket "$socket" -X POST http://localhost/asagiri/sleep; }
restore() { post "$original" && sleep_vm; }
trap restore EXIT
for size in 32 0; do
    settings=$(printf '%s' "$original" | python3 -c 'import json,sys; d=json.load(sys.stdin); d["swap_mib"]=int(sys.argv[1]); print(json.dumps(d))' "$size")
    post "$settings"
    sleep_vm
    docker -H "unix://$socket" run --pull=never --rm --network none "$image" sh -ec '
        awk -v size="$1" '\''$1 == "SwapTotal:" { found = 1; valid = size ? ($2 > (size-1)*1024 && $2 <= size*1024) : ($2 == 0); print } END {exit !(found && valid)}'\'' /proc/meminfo
    ' sh "$size"
done
