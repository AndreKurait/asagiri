#!/bin/sh
# Carry one reviewed upstream commit across the Kiri → Asagiri rename.
set -eu
exec python3 "$(dirname "$0")/port_upstream.py" "$@"
