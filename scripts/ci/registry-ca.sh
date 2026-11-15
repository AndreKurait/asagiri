#!/bin/sh
# Run the exact source's TLS/BuildKit probe against its already-built binaries.
set -eu
if [ "$#" -ne 2 ]; then
  echo "usage: registry-ca.sh BIN_DIR OUTPUT_DIR" >&2
  exit 2
fi
registry_source_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
registry_source_commit=$(git -C "$registry_source_root" rev-parse HEAD)
exec python3 "$registry_source_root/tests/registry-ca.py" qualify \
  --repository "$registry_source_root" --source "$registry_source_commit" \
  --bin "$1" --output "$2" --execute
