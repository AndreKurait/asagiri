#!/bin/bash
# API latency including curl and shell startup: one container lifecycle
# (create, start, wait, delete) and one exec, without the docker CLI.
#   SOCK=/run/dockerd-bench.sock bench/api.sh docker-engine
set -u
bench_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
label=$1; S=${SOCK:?}; OUT=${OUT:-$(dirname "$0")/results}; RUNS=${RUNS:-30}
c() { curl -s --unix-socket "$S" "$@"; }
cat > /tmp/api-run.sh <<R
c() { curl -s --unix-socket "$S" "\$@"; }
id=\$(c -X POST -H 'Content-Type: application/json' -d '{"Image":"alpine","Cmd":["true"]}' http://x/containers/create | jq -r .Id)
c -X POST http://x/containers/\$id/start >/dev/null; c -X POST http://x/containers/\$id/wait >/dev/null; c -X DELETE http://x/containers/\$id >/dev/null
R
id=$(c -X POST -H 'Content-Type: application/json' -d '{"Image":"alpine","Cmd":["sleep","600"]}' http://x/containers/create | jq -r .Id); c -X POST "http://x/containers/$id/start" >/dev/null
cat > /tmp/api-exec.sh <<R
c() { curl -s --unix-socket "$S" "\$@"; }
e=\$(c -X POST -H 'Content-Type: application/json' -d '{"Cmd":["true"],"AttachStdout":true}' http://x/containers/$id/exec | jq -r .Id)
c -X POST -H 'Content-Type: application/json' -d '{}' http://x/exec/\$e/start >/dev/null
R
hyperfine --warmup 3 --runs "$RUNS" --export-json "$OUT/$label.api-run.json" 'bash /tmp/api-run.sh' >/dev/null 2>&1
hyperfine --ignore-failure --warmup 3 --runs "$RUNS" --export-json "$OUT/$label.api-exec.json" 'bash /tmp/api-exec.sh' >/dev/null 2>&1
c -X DELETE "http://x/containers/$id?force=true" >/dev/null
run_stats=$(jq -L "$bench_dir" -r 'include "hyperfine"; hyperfine_wall_clock | "\(.median*1000|round) \(.min*1000|round)"' "$OUT/$label.api-run.json") || exit 1
exec_stats=$(jq -L "$bench_dir" -r 'include "hyperfine"; hyperfine_wall_clock | "\(.median*1000|round) \(.min*1000|round)"' "$OUT/$label.api-exec.json") || exit 1
read -r run_median run_min <<< "$run_stats"
read -r exec_median exec_min <<< "$exec_stats"
printf '%-14s API run %4s ms (min %4s)   API exec %4s ms (min %4s)\n' "$label" "$run_median" "$run_min" "$exec_median" "$exec_min"
