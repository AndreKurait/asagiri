#!/bin/sh
# INPUT.tar is an actual Alpine 3.21 checkpoint captured on the source engine.
set -eu
umask 077
asagiri=${ASAGIRI_BIN:-asagiri}
input=${1:?supply the source checkpoint archive}
work=$(mktemp -d)
cid=''
other=''
image=''
socket=${ASAGIRI_SOCKET:?set ASAGIRI_SOCKET to the destination engine socket}
cleanup() {
    if [ -n "$cid" ]; then docker rm -f "$cid" >/dev/null || true; fi
    if [ -n "$other" ]; then docker rm -f "$other" >/dev/null || true; fi
    if [ -n "$image" ]; then docker image rm "$image" >/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
cid=$(docker create --pull=never --network none alpine:3.21 sleep 300)
# Disconnect after sending a complete tar but before the declared HTTP body ends.
bytes=$(($(wc -c <"$input") + 0))
rc=0
curl --unix-socket "$socket" --silent --max-time 3 -X PUT -H 'Expect: 100-continue' \
    -H "Content-Length: $((bytes + 512))" --data-binary "@$input" -o "$work/result" \
    -D "$work/headers" -w '%{size_upload}' "http://localhost/asagiri/checkpoints/$cid/transfer" >"$work/sent" || rc=$?
test "$rc" -eq 28
grep -Eq '^HTTP/1\.[01] 100 ' "$work/headers"
test "$(cat "$work/sent")" -eq "$bytes"
test -z "$("$asagiri" checkpoint ls "$cid")"
# A container created from different image layers cannot receive this writable layer.
mkdir "$work/base"
printf 'another root' >"$work/base/file"
COPYFILE_DISABLE=1 tar --format=ustar -cf "$work/base.tar" -C "$work/base" file
image=$(docker import "$work/base.tar")
other=$(docker create --pull=never "$image" /bin/true)
if "$asagiri" checkpoint import "$other" transfer "$input" >"$work/result" 2>&1; then exit 1; fi
grep -F 'destination image layers differ' "$work/result"
test -z "$("$asagiri" checkpoint ls "$other")"
# The second tar end block is required even when all member bytes arrived.
dd if="$input" of="$work/short.tar" bs=512 count=$((bytes / 512 - 1)) 2>/dev/null
if "$asagiri" checkpoint import "$cid" transfer "$work/short.tar" >"$work/result" 2>&1; then exit 1; fi
grep -F 'missing tar end blocks' "$work/result"
test -z "$("$asagiri" checkpoint ls "$cid")"
"$asagiri" checkpoint import "$cid" transfer "$input"
if "$asagiri" checkpoint import "$cid" transfer "$input" >"$work/result" 2>&1; then exit 1; fi
grep -F 'checkpoint already exists' "$work/result"
test "$("$asagiri" checkpoint ls "$cid")" = transfer
"$asagiri" checkpoint export "$cid" transfer "$work/returned.tar"
# Compare one pair of members at a time in the private temporary directory.
tar -tf "$input" | LC_ALL=C sort >"$work/source"
tar -tf "$work/returned.tar" | LC_ALL=C sort >"$work/destination"
cmp "$work/source" "$work/destination"
while IFS= read -r member; do
    case "$member" in */) continue;; esac
    tar -xOf "$input" "$member" >"$work/source-member"
    tar -xOf "$work/returned.tar" "$member" >"$work/destination-member"
    cmp "$work/source-member" "$work/destination-member"
done <"$work/source"
test "$(docker inspect --format '{{.State.Status}}' "$cid")" = created
printf 'PASS destination retained all %d checkpoint members and left the container in created state\n' "$(wc -l <"$work/source")"
