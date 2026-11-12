#!/bin/sh
# Stamp a development build using this repository's release series.
# No external update feed is queried. Dependency versions remain locked.
#   scripts/ci/test-version.sh
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
version=$(cat "$root/RELEASE_VERSION")
python3 "$root/scripts/ci/release.py" stamp "$version-dev.${GITHUB_RUN_NUMBER:-1}"
