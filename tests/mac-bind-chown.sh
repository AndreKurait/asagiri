#!/bin/sh
# Kiri #382 / Asagiri #183. Use an existing local image and a writable Mac share.
set -eu

context=${1:-asagiri}
image=${2:-alpine:3.22}
docker --context "$context" version --format '{{.Server.Version}}'
docker --context "$context" image inspect "$image" --format '{{.Id}}'
probe=$(mktemp -d "${ASAGIRI_BIND_TEST_DIR:-$HOME}/asagiri-chown.XXXXXX")
trap 'rm -rf "$probe"' EXIT
chmod 755 "$probe"
host_owner=$(stat -f '%u:%g' "$probe")

run() {
    docker --context "$context" run --rm --pull never --network none --user 0:0 \
        --mount "$1" "$image" sh -euc "$2"
}

workload='
    umask 022
    adduser -D -H -u 10001 probe
    adduser -D -H -u 10002 other
    printf "test\n" > /data/file
    ln /data/file /data/alias
    chown 10001:10001 /data/file
    observed=$(stat -c "%u:%g %a" /data/file)
    printf "after chown: %s\n" "$observed"
    chmod_status=0
    su probe -s /bin/sh -c "chmod 600 /data/alias" || chmod_status=$?
    printf "chmod as UID 10001: exit %s\n" "$chmod_status"
    [ "$observed" = "10001:10001 644" ]
    [ "$chmod_status" -eq 0 ]
    su probe -s /bin/sh -c "test -r /data/alias"
    if su other -s /bin/sh -c "cat /data/alias"; then
        echo "UID 10002 could read a mode-0600 file" >&2
        exit 1
    fi
    [ "$(stat -c "%u:%g %a" /data/file)" = "10001:10001 600" ]
'

run 'type=tmpfs,destination=/data,tmpfs-mode=1777' "$workload"
bind="type=bind,source=$probe,destination=/data"
run "$bind" '
    awk '\''$5 == "/data" && / - virtiofs asagiri-mac / { print; found=1 }
        END { exit !found }'\'' /proc/self/mountinfo
'"$workload"
[ "$(stat -f '%u:%g' "$probe/file")" = "$host_owner" ]
run "$bind" '
    observed=$(stat -c "%u:%g %a" /data/alias)
    printf "after remount: %s\n" "$observed"
    [ "$observed" = "10001:10001 600" ]
    adduser -D -H -u 10001 probe
    su probe -s /bin/sh -c "chmod 640 /data/alias"
    [ "$(stat -c "%u:%g %a" /data/file)" = "10001:10001 640" ]
'
