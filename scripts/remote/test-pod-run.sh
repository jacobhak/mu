#!/usr/bin/env bash
# Runs pod-run.sh against a fake kubectl and a throwaway git repository. The fake's `exec` runs the command locally
# with HOME and the repository path redirected into a directory that stands in for the pod. A real cluster is
# needed for what this cannot show: pod scheduling, `kubectl wait`, jsonpath against real annotations, exec stream
# behaviour on disconnect, and the image itself.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd -P)

T=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/mu-pod-test.XXXXXX")" && pwd -P)
trap 'rm -rf "$T"' EXIT
FAKE=$T/fake
ROOT=$T/repo
mkdir -p "$FAKE/pod/home" "$FAKE/pod/root" "$ROOT/scripts/remote"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}
has() { grep -qF -- "$2" "$1" || fail "$1 lacks '$2'"; }
lacks() { ! grep -qiF -- "$2" "$1" || fail "$1 has '$2'"; }

cat > "$T/kubectl" << 'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "$FAKE/calls"
if [ "$1" = -n ]; then shift 2; fi
cmd=$1
shift
case $cmd in
  create) cat > "$FAKE/pod.yaml" ;;
  wait) ;;
  get) [ ! -f "$FAKE/pods.tsv" ] || cat "$FAKE/pods.tsv" ;;
  delete) echo "$2" >> "$FAKE/deletes" ;;
  exec)
    args=()
    for a in "$@"; do args+=("${a//$MU_TASK_ROOT/$FAKE/pod/root}"); done
    while [ "${args[0]}" != -- ]; do args=("${args[@]:1}"); done
    args=("${args[@]:1}")
    cd "$FAKE/pod"
    export HOME=$FAKE/pod/home
    case " ${args[*]} " in
      *' eval '*) touch "$FAKE/worker-started" ;;
    esac
    case "${args[*]}" in
      *'tar -xf'*) tee "$FAKE/config.tar" | "${args[@]}" ;;
      *) "${args[@]}" ;;
    esac
    ;;
  *) echo "fake kubectl: unexpected $cmd" >&2; exit 1 ;;
esac
EOF
chmod +x "$T/kubectl"

cp "$here/stage-a-worker.sh" "$ROOT/scripts/remote/"
git -C "$ROOT" init -q
echo hello > "$ROOT/a.txt"
git -C "$ROOT" add -A
git -C "$ROOT" -c user.name=t -c user.email=t@t commit -q -m init
mk_task_ref() {
  local c
  c=$(git -C "$ROOT" -c user.name=mu -c user.email=mu@localhost commit-tree "HEAD^{tree}" -p HEAD -m task)
  git -C "$ROOT" update-ref "refs/mu/task/$1" "$c"
}

mkdir -p "$T/mu-home" "$T/profile" "$T/agent/skills/s" "$T/agent/agents"
echo 'x = 1' > "$T/mu-home/config.toml"
echo '{"key":"secret"}' > "$T/mu-home/auth.json"
echo 'm = 1' > "$T/profile/models.toml"
echo '{"key":"secret"}' > "$T/profile/auth.json"
echo agents > "$T/agent/AGENTS.md"
echo skill > "$T/agent/skills/s/SKILL.md"
echo secret > "$T/agent/skills/s/token.txt"
echo '{}' > "$T/agent/auth.json"

# Runs the launcher as mu would: task id $1, extra environment after it. Output in $T/out, $T/err.
launch() {
  local id=$1
  shift
  mk_task_ref "$id"
  env FAKE="$FAKE" KUBECTL="$T/kubectl" MU_POD_LOG="$T/log.jsonl" MU_POD_SPEC="$here/pod.yaml" \
    MU_TASK_DEPTH=1 MU_TASK_NAME="$id" MU_TASK_ROOT="$ROOT" MU_TASK_CWD="$ROOT" \
    MU_TASK_REF="refs/mu/task/$id" MU_TASK_RESULT_REF="refs/mu/result/$id" MU_TASK_SESSION=sess \
    MU_TASK_PARENT_PID=$$ MU_TASK_MU_HOME="$T/mu-home" MU_TASK_PROFILE_HOME="$T/profile" MU_TASK_AGENT_DIR="$T/agent" \
    "$@"
}

fresh_pod() {
  rm -rf "$FAKE/pod" "$FAKE/deletes" "$FAKE/worker-started" "$FAKE/pods.tsv" "$FAKE/config.tar"
  mkdir -p "$FAKE/pod/home" "$FAKE/pod/root"
}

# 1. Normal run.
fresh_pod
printf '{"prompt":"hi"}\n' | launch norm-1 MU_POD_CMD="$ROOT/scripts/remote/stage-a-worker.sh" MU_STAGE_A_CHECKS=true \
  "$here/pod-run.sh" > "$T/out" 2> "$T/err" || fail "launcher failed: $(cat "$T/err")"
[ "$(wc -l < "$T/out" | tr -d ' ')" = 3 ] || fail "stdout is not three lines: $(cat "$T/out")"
sed -n 1p "$T/out" | grep -q '^{"type":"tool"' || fail "line 1 is not the tool line"
sed -n 2p "$T/out" | grep -q '^{"type":"result"' || fail "line 2 is not the result line"
sed -n 3p "$T/out" | grep -q '^{"type":"changes","ref":"refs/mu/result/norm-1","base":"[0-9a-f]\{40\}","files":1}$' ||
  fail "line 3 is not the changes line: $(sed -n 3p "$T/out")"
git -C "$ROOT" show "refs/mu/result/norm-1:stage-a-output.txt" > /dev/null || fail "result ref lacks the changed file"
has "$FAKE/deletes" norm-1
has "$FAKE/pod.yaml" 'mu/host: "'"$(hostname)"'"'
has "$FAKE/pod.yaml" "mu/pid: \"$$\""
has "$FAKE/pod.yaml" 'mu/parent: "sess"'
has "$FAKE/pod.yaml" 'name: "norm-1"'
grep -q '@' "$FAKE/pod.yaml" && fail "unfilled placeholder in pod.yaml"
grep -q 'delete pod norm-1 --wait=false' "$FAKE/calls" || fail "pod not deleted with --wait=false"
[ "$(wc -l < "$T/log.jsonl" | tr -d ' ')" = 1 ] || fail "expected one timing line"
has "$T/log.jsonl" '"exit":0'

# Config tar: what is sent, and no credentials.
tar -tf "$FAKE/config.tar" > "$T/tar.list"
has "$T/tar.list" .mu/config.toml
has "$T/tar.list" .mu/models.toml
has "$T/tar.list" .omp/agent/AGENTS.md
has "$T/tar.list" .omp/agent/skills/s/SKILL.md
lacks "$T/tar.list" './'
lacks "$T/tar.list" auth.json
lacks "$T/tar.list" token

# 2. TERM deletes the pod. The launcher gets its own process group, as mu's terminate_group assumes.
fresh_pod
set -m
printf '{"prompt":"hi"}\n' | launch term-1 MU_POD_CMD='sleep 30' "$here/pod-run.sh" > "$T/out2" 2> "$T/err2" &
pgid=$!
set +m
for _ in $(seq 1 100); do
  [ -f "$FAKE/worker-started" ] && break
  sleep 0.1
done
[ -f "$FAKE/worker-started" ] || fail "worker never started: $(cat "$T/err2")"
kill -TERM -- "-$pgid"
wait "$pgid" || true
has "$FAKE/deletes" term-1
has "$T/log.jsonl" '"exit":143'
git -C "$ROOT" rev-parse -q --verify refs/mu/result/term-1 > /dev/null && fail "TERM run produced a result ref"

# 3. Sweep: only this host's pods whose pid is dead.
fresh_pod
true &
dead=$!
wait "$dead"
printf 'orphan-1|%s|%s\nlive-1|%s|%s\nother-1|elsewhere|%s\nnopid-1|%s|\n' \
  "$(hostname)" "$dead" "$(hostname)" "$$" "$dead" "$(hostname)" > "$FAKE/pods.tsv"
printf '{"prompt":"hi"}\n' | launch sweep-1 MU_POD_CMD='true' "$here/pod-run.sh" > "$T/out3" 2> "$T/err3" ||
  fail "launcher failed: $(cat "$T/err3")"
has "$FAKE/deletes" orphan-1
has "$FAKE/deletes" sweep-1
lacks "$FAKE/deletes" live-1
lacks "$FAKE/deletes" other-1
lacks "$FAKE/deletes" nopid-1
grep -q 'delete pod orphan-1 --wait=false' "$FAKE/calls" || fail "orphan not deleted with --wait=false"
grep -q 'type' "$T/out3" && fail "stdout not empty for a worker that prints nothing: $(cat "$T/out3")"

echo "test-pod-run: ok"
