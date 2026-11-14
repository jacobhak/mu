#!/usr/bin/env bash
# Stage A scripted worker: set MU_POD_CMD to this script's path in the pod. Reads the task line, runs the CI checks,
# changes one file and prints a `result` line in the child NDJSON protocol. No model is involved.
# MU_STAGE_A_CHECKS replaces the checks command (a test hook).
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

IFS= read -r _task
checks=${MU_STAGE_A_CHECKS:-scripts/ci-checks.sh}
printf '{"type":"tool","name":"bash","summary":"%s"}\n' "$checks"

start=$(date +%s)
is_error=false
# stdout belongs to the protocol.
"$checks" >&2 || is_error=true
secs=$(($(date +%s) - start))

printf 'stage A: checks took %s s on %s\n' "$secs" "$(hostname)" > stage-a-output.txt
printf '{"type":"result","text":"checks took %s s","isError":%s,"session":"","usage":{"input":0,"output":0}}\n' "$secs" "$is_error"
