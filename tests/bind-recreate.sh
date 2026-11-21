#!/bin/bash
# A name cached by virtiofs must be reusable after the Mac deletes or
# replaces the file (#399), with and without an inotify watcher:
#   ASAGIRI_SOCKET=/path/to/private/docker.sock tests/bind-recreate.sh
set -eu

if [ "$(uname -s)" != Darwin ] || [ ! -S "${ASAGIRI_SOCKET:-}" ]; then
  echo "Run against a disposable macOS Asagiri engine with ASAGIRI_SOCKET set." >&2
  exit 2
fi
unset DOCKER_CONTEXT
cli() { docker --host "unix://$ASAGIRI_SOCKET" "$@"; }
cli version --format '{{range .Server.Components}}{{index .Details "AsagiriVersion"}}{{end}}' | grep -q '[0-9]'
image=alpine:3.21
cli image inspect "$image" >/dev/null

bind=$(mktemp -d)
bind=$(cd "$bind" && pwd -P)
container="asagiri-bind-recreate-${bind##*/}"
cleanup() {
  cli rm -f "$container" >/dev/null 2>&1 || true
  rm -rf "$bind"
}
trap cleanup EXIT
failed=0
cat > "$bind/onchange.sh" <<'SH'
#!/bin/sh
cat "$2/$3" > "/tmp/seen-$3" 2>&1
SH
chmod +x "$bind/onchange.sh"

for watched in no yes; do
  if [ "$watched" = yes ]; then
    cli run -d --pull never --network none --name "$container" -v "$bind:/w" "$image" inotifyd /w/onchange.sh /w:c >/dev/null
  else
    cli run -d --pull never --network none --name "$container" -v "$bind:/w" "$image" sleep 60 >/dev/null
  fi
  # The daemon scans for new bind sources once a second. Let FSEvents
  # subscribe before testing changes, so this tests the guest's cache.
  sleep 2
  for change in guest-create host-replace host-recreate; do
    for i in 1 2 3 4 5; do
      file="$watched-$change-$i"
      echo old > "$bind/$file"
      cli exec "$container" cat "/w/$file" >/dev/null
      if [ "$watched" = yes ]; then
        sleep 0.1
        cli exec "$container" rm -f "/tmp/seen-$file"
      fi
      case "$change" in
        guest-create) rm "$bind/$file" ;;
        host-replace) echo new > "$bind/tmp"; mv "$bind/tmp" "$bind/$file" ;;
        host-recreate) rm "$bind/$file"; echo new > "$bind/$file" ;;
      esac
      # FSEvents batches changes for 50 ms. Allow delivery, but stay well
      # inside Apple's roughly one-second positive-entry cache timeout.
      sleep 0.2
      if [ "$change" = guest-create ] &&
         ! cli exec "$container" sh -c 'echo new > "/w/$1"' sh "$file"; then
        echo "FAIL $change (watcher: $watched, file: $file)" >&2
        failed=1
        continue
      fi
      # The watcher's first read after the event must already see the
      # replacement, before the explicit read below can refresh its name.
      if [ "$watched" = yes ]; then
        if ! content=$(cli exec "$container" sh -c '
          for attempt in 1 2 3 4 5 6 7 8 9 10; do
            if [ -f "/tmp/seen-$1" ]; then cat "/tmp/seen-$1"; exit; fi
            sleep 0.02
          done
          exit 1' sh "$file") || [ "$content" != new ]; then
          echo "FAIL watcher read after $change (file: $file): ${content:-no event}" >&2
          failed=1
        fi
      fi
      if ! content=$(cli exec "$container" cat "/w/$file") ||
         [ "$content" != new ] || [ "$(cat "$bind/$file" 2>/dev/null)" != new ]; then
        echo "FAIL $change (watcher: $watched, file: $file)" >&2
        failed=1
      fi
    done
  done
  cli rm -f "$container" >/dev/null
done
if [ "$failed" = 0 ]; then
  echo "PASS bind recreate and replace, with and without a watcher (30 attempts)"
fi
exit "$failed"
