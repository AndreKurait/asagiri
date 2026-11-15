#!/bin/sh
# The controller runs on the host so the pinned offline fixture needs no Python.
# Usage: bridge-start.sh BIN_DIR DAEMON_FIXTURE PINNED_IMAGE --output REPORT
exec python3 "$(dirname "$0")/bridge-start.py" "$@"
