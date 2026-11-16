#!/bin/bash
# Turns bench/results/*.json (git-ignored; finished runs are kept outside this repository) into a markdown table (medians; MB/s where it is a transfer).
bench_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
shopt -s nullglob
OUT=${OUT:-$(dirname "$0")/results}
cat <<'TABLE'
| Engine | `run` ms | `exec` ms | stdout MB/s | stdin MB/s | bind write MB/s | bind read MB/s | net Gbit/s | build ms | pull ms |
|---|---|---|---|---|---|---|---|---|---|
TABLE
for n in "$OUT"/*.net.json; do
  l=$(jq -r .label "$n"); J="$OUT/$l"
  med() { [ -f "$J.$1.json" ] && jq -L "$bench_dir" -r 'include "hyperfine"; hyperfine_wall_clock | .median * 1000 | round' "$J.$1.json" || echo "n/a"; }
  mbs() { [ -f "$J.$1.json" ] && jq -L "$bench_dir" --argjson bytes "$2" -r 'include "hyperfine"; hyperfine_wall_clock | $bytes / (1000000 * .median) | round' "$J.$1.json" || echo "n/a"; }
  echo "| $l | $(med run) | $(med exec) | $(mbs stdout 200000000) | $(mbs stdin 200000000) | $(mbs bind-write 1073741824) | $(mbs bind-read 1073741824) | $(jq -r .net_gbps "$n") | $(med build) | $(med pull) |"
done
