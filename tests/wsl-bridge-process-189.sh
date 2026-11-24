#!/bin/sh
set -eu
pid=$(pgrep -f '^/usr/local/bin/asagiri bridge .*docker[.]sock$')
# This disposable distro serves exactly one Windows launcher.
[ "$(printf '%s\n' "$pid" | wc -l)" -eq 1 ]
sha256sum "/proc/$pid/exe" | cut -d ' ' -f 1
