#!/usr/bin/env bash
# Kubernetes launcher for mu task subagents: runs in place of `mu --headless` with the same stdin and stdout.
# Everything travels through `kubectl exec`: config as a tar, the repository as a git bundle, the protocol, and the
# result bundle back. Nothing but the protocol is written to stdout; diagnostics go to stderr. The `changes` report
# goes to file descriptor 3, which mu opens for a launcher; it is not written anywhere when descriptor 3 is closed.
#
# Input: the MU_TASK_* variables mu sets for a launcher (see the remote-execution design, Plan 1 step 3).
# Settings (environment):
#   KUBECTL                 kubectl binary (default kubectl); point it at a fake in tests
#   MU_POD_NAMESPACE        namespace (default default)
#   MU_POD_IMAGE            worker image (default mu-worker:latest)
#   MU_POD_PULL_POLICY      imagePullPolicy (default IfNotPresent)
#   MU_POD_CPU, MU_POD_MEMORY       resource requests (default 2, 4Gi)
#   MU_POD_DEADLINE         activeDeadlineSeconds (default 21600, 6 h)
#   MU_POD_READY_TIMEOUT    wait for the pod to be Ready (default 300s)
#   MU_POD_SCCACHE_SECRET   optional secret whose keys become the pod's environment (default mu-sccache)
#   MU_POD_CMD              command run in the pod in place of `mu --headless`; scripts/remote/stage-a-worker.sh
#   MU_POD_SPEC             pod template (default pod.yaml beside this script)
#   MU_POD_LOG              JSONL timing log (default ${TMPDIR:-/tmp}/mu-pod-run.jsonl)
# SCCACHE_BUCKET, SCCACHE_ENDPOINT, SCCACHE_REGION, SCCACHE_S3_USE_SSL and SCCACHE_S3_KEY_PREFIX, when set here,
# are set in the pod.
set -euo pipefail
# bash 5.2 treats `&` in a ${var//pat/rep} replacement specially.
shopt -u patsub_replacement 2> /dev/null || true

KUBECTL=${KUBECTL:-kubectl}
NAMESPACE=${MU_POD_NAMESPACE:-default}
IMAGE=${MU_POD_IMAGE:-mu-worker:latest}
SPEC=${MU_POD_SPEC:-$(dirname "$0")/pod.yaml}
LOG=${MU_POD_LOG:-${TMPDIR:-/tmp}/mu-pod-run.jsonl}
WORKER_CMD=${MU_POD_CMD:-mu --headless}

: "${MU_TASK_NAME:?}" "${MU_TASK_ROOT:?}" "${MU_TASK_REF:?}" "${MU_TASK_RESULT_REF:?}" "${MU_TASK_DEPTH:?}"
CWD=${MU_TASK_CWD:-$MU_TASK_ROOT}
POD=$MU_TASK_NAME
HOST=$(hostname)

log() { echo "pod-run: $*" >&2; }
kc() { "$KUBECTL" -n "$NAMESPACE" "$@"; }

now_ms() {
  if [ -n "${EPOCHREALTIME:-}" ]; then
    local s=${EPOCHREALTIME%[.,]*} f=${EPOCHREALTIME#*[.,]}
    echo $((s * 1000 + 10#${f:0:3}))
  else
    echo $(($(date +%s) * 1000))
  fi
}
bytes() { wc -c < "$1" | tr -d ' '; }
# Escapes a string for a double-quoted YAML scalar.
yq() {
  local s=${1//\\/\\\\}
  echo "${s//\"/\\\"}"
}

START=$(now_ms)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/mu-pod-run.XXXXXX")
POD_CREATED=0
READY_MS=null
REPO_MS=null
BYTES_OUT=0
BYTES_IN=0
WORKER_RC=0

finish() {
  local rc=$?
  trap - EXIT TERM INT HUP
  if [ "$POD_CREATED" = 1 ]; then
    kc delete pod "$POD" --wait=false --ignore-not-found > /dev/null 2>&1 || log "could not delete pod $POD"
  fi
  local first=null
  [ -s "$TMP/first" ] && first=$(($(cat "$TMP/first") - START))
  printf '{"ts":%s,"pod":"%s","host":"%s","pod_ready_ms":%s,"repo_ready_ms":%s,"first_line_ms":%s,"bundle_out_bytes":%s,"bundle_in_bytes":%s,"exit":%s}\n' \
    "$(date +%s)" "$POD" "$HOST" "$READY_MS" "$REPO_MS" "$first" "$BYTES_OUT" "$BYTES_IN" "$rc" >> "$LOG" 2> /dev/null || true
  rm -rf "$TMP"
  exit "$rc"
}
trap finish EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
trap 'exit 129' HUP

# 7. Sweep pods that a killed launcher left: this host's, with a parent pid that is gone.
sweep() {
  local out name host pid
  out=$(kc get pods -l app=mu-task -o 'jsonpath={range .items[*]}{.metadata.name}|{.metadata.annotations.mu/host}|{.metadata.annotations.mu/pid}{"\n"}{end}') || {
    log "orphan sweep: could not list pods"
    return 0
  }
  while IFS='|' read -r name host pid; do
    [ -n "$name" ] && [ "$host" = "$HOST" ] || continue
    case $pid in '' | *[!0-9]*) continue ;; esac
    if ! ps -p "$pid" > /dev/null 2>&1; then
      log "orphan sweep: deleting $name (pid $pid is gone)"
      kc delete pod "$name" --wait=false --ignore-not-found > /dev/null 2>&1 || true
    fi
  done <<< "$out"
}
sweep

# 1. The pod.
spec=$(cat "$SPEC")
sccache_env=
for v in SCCACHE_BUCKET SCCACHE_ENDPOINT SCCACHE_REGION SCCACHE_S3_USE_SSL SCCACHE_S3_KEY_PREFIX; do
  val=${!v:-}
  [ -z "$val" ] || sccache_env="${sccache_env}        - {name: $v, value: \"$(yq "$val")\"}"$'\n'
done
spec=${spec//@NAME@/$POD}
spec=${spec//@NAMESPACE@/$(yq "$NAMESPACE")}
spec=${spec//@IMAGE@/$(yq "$IMAGE")}
spec=${spec//@PULL_POLICY@/${MU_POD_PULL_POLICY:-IfNotPresent}}
spec=${spec//@PARENT@/$(yq "${MU_TASK_SESSION:-}")}
spec=${spec//@HOST@/$(yq "$HOST")}
spec=${spec//@PID@/${MU_TASK_PARENT_PID:-}}
spec=${spec//@DEADLINE@/${MU_POD_DEADLINE:-21600}}
spec=${spec//@CPU@/${MU_POD_CPU:-2}}
spec=${spec//@MEMORY@/${MU_POD_MEMORY:-4Gi}}
spec=${spec//@SCCACHE_SECRET@/${MU_POD_SCCACHE_SECRET:-mu-sccache}}
spec=${spec//@ROOT@/$(yq "$MU_TASK_ROOT")}
spec=${spec//@ENV@/${sccache_env%$'\n'}}

POD_CREATED=1
printf '%s\n' "$spec" | kc create -f - > /dev/null
kc wait --for=condition=Ready "pod/$POD" "--timeout=${MU_POD_READY_TIMEOUT:-300s}" > /dev/null
READY_MS=$(($(now_ms) - START))

# 2. Config: only the files that exist; never auth.json or a token file.
stage=$TMP/config
mkdir -p "$stage/.mu" "$stage/.omp/agent"
[ ! -f "${MU_TASK_MU_HOME:-}/config.toml" ] || cp "$MU_TASK_MU_HOME/config.toml" "$stage/.mu/"
for f in models.toml providers.toml; do
  [ ! -f "${MU_TASK_PROFILE_HOME:-}/$f" ] || cp "$MU_TASK_PROFILE_HOME/$f" "$stage/.mu/"
done
for f in AGENTS.md RULES.md; do
  [ ! -f "${MU_TASK_AGENT_DIR:-}/$f" ] || cp "$MU_TASK_AGENT_DIR/$f" "$stage/.omp/agent/"
done
for d in agents skills; do
  [ ! -d "${MU_TASK_AGENT_DIR:-}/$d" ] || cp -RL "$MU_TASK_AGENT_DIR/$d" "$stage/.omp/agent/"
done
find "$stage" \( -iname 'auth.json' -o -iname '*token*' \) -exec rm -rf {} + 2> /dev/null || true
# Without the "." entry: the worker owns the files but not the mount point, so extracting "." fails on its mode.
# shellcheck disable=SC2046
tar -cf - -C "$stage" -- $(ls -A "$stage") | kc exec -i "$POD" -- sh -c 'tar -xf - -C "$HOME"' > /dev/null

# 3. Repository: the task commit as a bundle, fetched into the same absolute path and checked out.
git -C "$MU_TASK_ROOT" bundle create - "$MU_TASK_REF" > "$TMP/out.bundle" 2> /dev/null
BYTES_OUT=$(bytes "$TMP/out.bundle")
kc exec -i "$POD" -- sh -c 'cat > "$HOME/task.bundle"' < "$TMP/out.bundle" > /dev/null
kc exec "$POD" -- sh -c '
  set -e
  cd "$1"
  git init -q
  git fetch -q "$HOME/task.bundle" "$2:$2"
  git checkout -q --detach "$2"
' sh "$MU_TASK_ROOT" "$MU_TASK_REF" < /dev/null > /dev/null
REPO_MS=$(($(now_ms) - START))

# 4. The protocol. The loop forwards lines and notes when the first one arrived.
set +e
kc exec -i "$POD" -- env "MU_TASK_DEPTH=$MU_TASK_DEPTH" \
  sh -c 'cd "$1" && eval "$2"' sh "$CWD" "$WORKER_CMD" \
  | while IFS= read -r line || [ -n "$line" ]; do
    [ -s "$TMP/first" ] || now_ms > "$TMP/first"
    printf '%s\n' "$line"
  done
WORKER_RC=${PIPESTATUS[0]}
set -e

# 5. Result: commit on top of the task commit; bring it back as a bundle only when something changed.
summary=$(kc exec "$POD" -- sh -c '
  set -e
  cd "$1"
  base=$(git rev-parse "$2")
  git add -A
  if git diff --cached --quiet; then exit 0; fi
  GIT_AUTHOR_NAME=mu GIT_AUTHOR_EMAIL=mu@localhost GIT_COMMITTER_NAME=mu GIT_COMMITTER_EMAIL=mu@localhost \
    git commit -q -m "mu task result"
  git update-ref "$3" HEAD
  git bundle create "$HOME/result.bundle" "$base..$3" > /dev/null 2>&1
  echo "$base $(git diff --name-only "$base" HEAD | wc -l | tr -d " ")"
' sh "$MU_TASK_ROOT" "$MU_TASK_REF" "$MU_TASK_RESULT_REF" < /dev/null) || summary=
if [ -n "$summary" ]; then
  kc exec "$POD" -- sh -c 'cat "$HOME/result.bundle"' < /dev/null > "$TMP/in.bundle"
  BYTES_IN=$(bytes "$TMP/in.bundle")
  git -C "$MU_TASK_ROOT" fetch -q "$TMP/in.bundle" "$MU_TASK_RESULT_REF:$MU_TASK_RESULT_REF" >&2
  base=${summary%% *}
  files=${summary##* }
  # Only mu's own descriptor 3 is trusted for this line: the worker's stdout could print the same text.
  printf '{"type":"changes","ref":"%s","base":"%s","files":%s}\n' "$MU_TASK_RESULT_REF" "$base" "$files" 2> /dev/null >&3 || true
fi

exit "$WORKER_RC"
