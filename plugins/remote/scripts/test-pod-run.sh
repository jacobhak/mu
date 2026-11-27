#!/usr/bin/env bash
# Runs adapters/mu-remote-pod against a fake kubectl, a fake `mu` and a fake `mu-remote`, with a throwaway git
# repository. Run it from anywhere; it exits non-zero on the first failed expectation. The fake's `exec` runs
# the command locally with HOME and the checkout path redirected into a directory that stands in for the pod. A
# real cluster is needed for what this cannot show: pod scheduling, `kubectl wait`, jsonpath against real labels,
# exec stream behaviour on disconnect, and the image itself.
set -euo pipefail
here=$(cd "$(dirname "$0")/../adapters" && pwd -P)

T=$(cd "$(mktemp -d "${TMPDIR:-/tmp}/mu-pod-test.XXXXXX")" && pwd -P)
trap 'rm -rf "$T"' EXIT
FAKE=$T/fake
ROOT=$T/repo
DIR=/mu-test-checkout
mkdir -p "$FAKE" "$ROOT" "$T/bin"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}
has() { grep -qaF -- "$2" "$1" || fail "$1 lacks '$2'"; }
lacks() { ! grep -qaiF -- "$2" "$1" || fail "$1 has '$2'"; }

cat > "$T/kubectl" << 'EOF'
#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "$FAKE/calls"
while [ "$1" = -n ] || [ "$1" = --context ]; do shift 2; done
cmd=$1
shift
case $cmd in
  create)
    cat > "$FAKE/pod.yaml"
    name=$(sed -n 's/^  name: "\(.*\)"$/\1/p' "$FAKE/pod.yaml")
    kind=$(sed -n 's/^    mu-kind: "\(.*\)"$/\1/p' "$FAKE/pod.yaml")
    launcher=$(sed -n 's/^    mu-launcher: "\(.*\)"$/\1/p' "$FAKE/pod.yaml")
    echo "$name|$kind|$launcher|Running" >> "$FAKE/pods.tsv"
    ;;
  wait) [ ! -f "$FAKE/wait-fails" ] || { echo "timed out waiting" >&2; exit 1; } ;;
  get) [ ! -f "$FAKE/pods.tsv" ] || cat "$FAKE/pods.tsv" ;;
  delete)
    echo "$2" >> "$FAKE/deletes"
    [ ! -f "$FAKE/pods.tsv" ] || { grep -v "^$2|" "$FAKE/pods.tsv" > "$FAKE/pods.new" || true; mv "$FAKE/pods.new" "$FAKE/pods.tsv"; }
    ;;
  exec)
    pod=
    args=()
    for a in "$@"; do args+=("${a//$DIR/$FAKE/pod/root}"); done
    while [ "${args[0]}" != -- ]; do
      case ${args[0]} in -*) ;; *) pod=${args[0]} ;; esac
      args=("${args[@]:1}")
    done
    args=("${args[@]:1}")
    echo "$pod ${args[*]}" >> "$FAKE/execs"
    mkdir -p "$FAKE/pod/home" "$FAKE/pod/root"
    cd "$FAKE/pod"
    export HOME=$FAKE/pod/home FAKE_POD=$pod
    case "${args[*]}" in
      *'tar -xf'*) tee "$FAKE/config.tar" | "${args[@]}" ;;
      *) "${args[@]}" ;;
    esac
    ;;
  port-forward)
    last=${!#}
    if [ -f "$FAKE/forward-fails" ]; then
      echo "error: unable to listen on port ${last%%:*}: address already in use" >&2
      exit 1
    fi
    echo "Forwarding from 127.0.0.1:${last%%:*} -> ${last##*:}"
    exec sleep 300
    ;;
  *) echo "fake kubectl: unexpected $cmd" >&2; exit 1 ;;
esac
EOF
cat > "$T/bin/mu" << 'EOF'
#!/usr/bin/env bash
echo "$*" >> "$FAKE/mu-calls"
case "$1 $2" in
  "agent list") [ ! -f "$FAKE/list-$FAKE_POD" ] || cat "$FAKE/list-$FAKE_POD" ;;
  "agent --detach") echo '{"id":"agent-9"}' ;;
  "agent stop") rm -f "$FAKE/list-$FAKE_POD" ;;
  "agent cancel") ;;
  "notify "*) cat >> "$FAKE/notified"; echo 'queued (x)' ;;
esac
EOF
cat > "$T/bin/mu-remote" << 'EOF'
#!/usr/bin/env bash
echo "$*" >> "$FAKE/mu-remote-calls"
case "$1 $2" in
  "worker bundle")
    [ ! -f "$FAKE/bundle-fails" ] || { echo "nothing to bundle" >&2; exit 1; }
    printf 'BUNDLE-BYTES' ;;
  "bridge "*) cat; exit 7 ;;
esac
EOF
chmod +x "$T/kubectl" "$T/bin/mu" "$T/bin/mu-remote"

git -C "$ROOT" init -q
echo hello > "$ROOT/a.txt"
git -C "$ROOT" add -A
git -C "$ROOT" -c user.name=t -c user.email=t@t commit -q -m init
mk_bundle() {
  local c
  c=$(git -C "$ROOT" -c user.name=mu -c user.email=mu@localhost commit-tree "HEAD^{tree}" -p HEAD -m task)
  git -C "$ROOT" update-ref "refs/mu/task/$1" "$c"
  git -C "$ROOT" bundle create "$T/$1.bundle" "refs/mu/task/$1" > /dev/null 2>&1
}

mkdir -p "$T/home" "$T/mu-home" "$T/agent/skills/s" "$T/agent/agents"
echo 'x = 1' > "$T/mu-home/config.toml"
echo '{"key":"TOPSECRET"}' > "$T/mu-home/auth.json"
echo 'm = 1' > "$T/mu-home/models.toml"
echo '{"key":"TOPSECRET"}' > "$T/mu-home/mu-token.json"
echo agents > "$T/agent/AGENTS.md"
echo skill > "$T/agent/skills/s/SKILL.md"
echo TOPSECRET > "$T/agent/skills/s/token.txt"
echo '{"key":"TOPSECRET"}' > "$T/agent/auth.json"

TRUSTED=0
SETTINGS='{"namespace":"ns","agent_namespace":"agents","image":"img:1"}'
# Runs the adapter as mu would. Output in $T/out and $T/err; returns its status.
adapter() {
  env -u MU_PROFILE -u OMP_PROFILE -u PI_PROFILE -u SCCACHE_BUCKET -u SCCACHE_ENDPOINT -u SCCACHE_REGION -u SCCACHE_S3_USE_SSL \
    -u SCCACHE_S3_KEY_PREFIX FAKE="$FAKE" KUBECTL="$T/kubectl" PATH="$T/bin:$PATH" MU_POD_SPEC="$here/mu-remote-pod.yaml" \
    MU_REMOTE_NAME=pods MU_REMOTE_SETTINGS="$SETTINGS" MU_REMOTE_TRUSTED="$TRUSTED" DIR="$DIR" \
    HOME="$T/home" MU_HOME="$T/mu-home" MU_REMOTE_HOME="$T/state" PI_CODING_AGENT_DIR="$T/agent" \
    "$here/mu-remote-pod" "$@" > "$T/out" 2> "$T/err"
}
fresh() {
  rm -rf "$FAKE"
  mkdir -p "$FAKE"
}

# version
fresh
adapter version || fail "version failed: $(cat "$T/err")"
[ "$(cat "$T/out")" = '{"contract":2}' ] || fail "version printed: $(cat "$T/out")"

# start --kind worker on an untrusted remote: progress, no secret, the checkout and the worker.
fresh
mk_bundle w1
adapter start --kind worker --launch w1 --bundle "$T/w1.bundle" --dir "$DIR" || fail "start failed: $(cat "$T/err")"
[ "$(cat "$T/out")" = '{"id":"w1"}' ] || fail "start printed: $(cat "$T/out")"
[ "$(grep -c '^{"progress"' "$T/err")" = 4 ] || fail "expected four progress lines: $(cat "$T/err")"
[ "$(grep '^{"progress"' "$T/err" | paste -sd' ' -)" = '{"progress":"scheduling"} {"progress":"pulling image"} {"progress":"copying repository"} {"progress":"starting"}' ] ||
  fail "progress order: $(cat "$T/err")"
has "$FAKE/pod.yaml" 'name: "w1"'
has "$FAKE/pod.yaml" 'namespace: "ns"'
has "$FAKE/pod.yaml" 'mu-kind: "worker"'
has "$FAKE/pod.yaml" 'image: "img:1"'
has "$FAKE/pod.yaml" "mountPath: \"$DIR\""
grep -q '@' "$FAKE/pod.yaml" && fail "unfilled placeholder in pod.yaml"
lacks "$FAKE/pod.yaml" secretRef
lacks "$FAKE/pod.yaml" mu-sccache
has "$FAKE/pod.yaml" 'activeDeadlineSeconds: 21600'
[ -s "$T/state/launcher-id" ] || fail "no launcher id"
has "$FAKE/pod.yaml" "mu-launcher: \"$(cat "$T/state/launcher-id")\""
[ -f "$FAKE/pod/root/a.txt" ] || fail "the repository was not checked out"
has "$FAKE/mu-remote-calls" "worker run --launch w1 --dir $FAKE/pod/root --detach"
[ ! -f "$FAKE/deletes" ] || fail "start deleted a pod: $(cat "$FAKE/deletes")"
tar -tf "$FAKE/config.tar" > "$T/tar.list"
has "$T/tar.list" .mu/config.toml
has "$T/tar.list" .mu/models.toml
has "$T/tar.list" .omp/agent/AGENTS.md
has "$T/tar.list" .omp/agent/skills/s/SKILL.md
lacks "$T/tar.list" './'
lacks "$T/tar.list" auth.json
lacks "$T/tar.list" token
# Everything kubectl received: its arguments, the pod, the config tar, the uploaded bundle.
for f in "$FAKE/calls" "$FAKE/pod.yaml" "$FAKE/config.tar" "$FAKE/pod/home/task.bundle"; do lacks "$f" TOPSECRET; done
id1=$(cat "$T/state/launcher-id")

# connect: through kubectl exec and `mu-remote bridge`; a dropped stream deletes nothing.
rc=0
echo '{"hello":1}' | adapter connect w1 || rc=$?
[ "$rc" = 7 ] || fail "connect did not return the stream's status: $rc"
has "$T/out" '{"hello":1}'
has "$FAKE/calls" 'exec -i w1 -- mu-remote bridge w1'
has "$FAKE/mu-remote-calls" 'bridge w1'
[ ! -f "$FAKE/deletes" ] || fail "a dropped stream deleted a pod: $(cat "$FAKE/deletes")"

# fetch: the bundle on stdout, the pod deleted afterwards; a failed fetch keeps the pod.
touch "$FAKE/bundle-fails"
adapter fetch w1 && fail "fetch succeeded without a bundle"
[ ! -f "$FAKE/deletes" ] || fail "a failed fetch deleted the pod"
rm "$FAKE/bundle-fails"
adapter fetch w1 || fail "fetch failed: $(cat "$T/err")"
[ "$(cat "$T/out")" = BUNDLE-BYTES ] || fail "fetch printed: $(cat "$T/out")"
has "$FAKE/deletes" w1
grep -q 'delete pod w1 --wait=false' "$FAKE/calls" || fail "pod not deleted with --wait=false"
[ "$(grep -n 'worker bundle' "$FAKE/mu-remote-calls" | tail -1 | cut -d: -f1)" -ge 1 ] || fail "no bundle call"

# stop: the worker is told, the pod deleted.
fresh
mk_bundle w2
adapter start --kind worker --launch w2 --bundle "$T/w2.bundle" --dir "$DIR" || fail "start failed: $(cat "$T/err")"
adapter stop w2 || fail "stop failed: $(cat "$T/err")"
has "$FAKE/mu-remote-calls" 'worker stop w2'
has "$FAKE/deletes" w2

# stop, cancel and notify of an agent: an adapter-made pod goes with its agent, a manifest pod stays.
fresh
printf 'mu-agent-7|agent|%s|Running\n' "$id1" > "$FAKE/pods.tsv"
echo '{"id":"agent-9","cwd":"/p"}' > "$FAKE/list-mu-agent-7"
adapter stop agent-9 || fail "agent stop failed: $(cat "$T/err")"
has "$FAKE/mu-calls" 'agent stop agent-9'
has "$FAKE/deletes" mu-agent-7
lacks "$FAKE/mu-calls" 'worker stop'
fresh
printf 'mu-agent-0|agent||Running\n' > "$FAKE/pods.tsv"
echo '{"id":"agent-1","cwd":"/p"}' > "$FAKE/list-mu-agent-0"
adapter stop agent-1 || fail "manifest agent stop failed: $(cat "$T/err")"
has "$FAKE/mu-calls" 'agent stop agent-1'
[ ! -f "$FAKE/deletes" ] || fail "stop deleted a pod this adapter did not make: $(cat "$FAKE/deletes")"
echo '{"id":"agent-1","cwd":"/p"}' > "$FAKE/list-mu-agent-0"
adapter cancel agent-1 || fail "cancel failed: $(cat "$T/err")"
has "$FAKE/mu-calls" 'agent cancel agent-1'
echo hello | adapter notify agent-1 || fail "notify failed: $(cat "$T/err")"
[ "$(cat "$T/out")" = 'queued (x)' ] || fail "notify printed: $(cat "$T/out")"
has "$FAKE/notified" hello
has "$FAKE/mu-calls" 'notify agent-1 -'

# A failed start deletes the pod it made.
fresh
mk_bundle w3
touch "$FAKE/wait-fails"
adapter start --kind worker --launch w3 --bundle "$T/w3.bundle" --dir "$DIR" && fail "start succeeded with a pod that never got ready"
has "$T/err" 'timed out waiting'
has "$FAKE/deletes" w3

# An untrusted remote refuses an sccache secret and nothing is created; a trusted one mounts it.
fresh
SETTINGS='{"sccache_secret_name":"cache-keys"}' adapter start --kind worker --launch w4 --bundle "$T/w3.bundle" --dir "$DIR" &&
  fail "an untrusted remote accepted a secret"
has "$T/err" 'not trusted'
[ "$(wc -l < "$T/err" | tr -d ' ')" = 1 ] || fail "the refusal is not one line: $(cat "$T/err")"
[ ! -f "$FAKE/calls" ] || fail "kubectl was called: $(cat "$FAKE/calls")"
TRUSTED=1
SETTINGS='{"sccache_secret_name":"cache-keys"}' adapter start --kind worker --launch w5 --bundle "$T/w3.bundle" --dir "$DIR" ||
  fail "trusted start failed: $(cat "$T/err")"
has "$FAKE/pod.yaml" 'name: "cache-keys"'
TRUSTED=1 adapter start --kind worker --launch w6 --bundle "$T/w3.bundle" --dir "$DIR" || fail "$(cat "$T/err")"
has "$FAKE/pod.yaml" 'name: "mu-sccache"'
TRUSTED=0

# start --kind agent: the agent namespace, mu's own reply, arguments passed on.
fresh
adapter start --kind agent --dir "$DIR" --model p/m --resume abc || fail "agent start failed: $(cat "$T/err")"
[ "$(cat "$T/out")" = '{"id":"agent-9"}' ] || fail "agent start printed: $(cat "$T/out")"
has "$FAKE/pod.yaml" 'mu-kind: "agent"'
has "$FAKE/pod.yaml" 'namespace: "agents"'
lacks "$FAKE/pod.yaml" activeDeadlineSeconds
has "$FAKE/mu-calls" 'agent --detach --model p/m --resume abc'
grep -q '^-n agents create' "$FAKE/calls" || fail "agent pod not made in the agent namespace"
adapter start --kind agent --dir "$DIR" -c || fail "$(cat "$T/err")"
has "$FAKE/mu-calls" 'agent --detach -c'
SETTINGS='{"agent_deadline":"7200"}' adapter start --kind agent --dir "$DIR" || fail "$(cat "$T/err")"
has "$FAKE/pod.yaml" 'activeDeadlineSeconds: 7200'

# start --kind agent --bundle: the repository is cloned into the directory (history and branch), nothing secret is sent.
fresh
git -C "$ROOT" branch -q -M main
echo second > "$ROOT/b.txt"
git -C "$ROOT" add -A
git -C "$ROOT" -c user.name=t -c user.email=t@t commit -q -m second
git -C "$ROOT" bundle create "$T/agent.bundle" HEAD refs/heads/main > /dev/null 2>&1
adapter start --kind agent --bundle "$T/agent.bundle" --dir "$DIR" || fail "agent start with a bundle failed: $(cat "$T/err")"
[ "$(cat "$T/out")" = '{"id":"agent-9"}' ] || fail "agent start printed: $(cat "$T/out")"
[ -f "$FAKE/pod/root/a.txt" ] && [ -f "$FAKE/pod/root/b.txt" ] || fail "the repository was not cloned"
[ "$(git -C "$FAKE/pod/root" rev-list --count HEAD)" = 2 ] || fail "the clone lacks the history"
[ "$(git -C "$FAKE/pod/root" branch --show-current)" = main ] || fail "the clone is not on the branch"
has "$FAKE/mu-calls" 'agent --detach'
for f in "$FAKE/calls" "$FAKE/pod.yaml" "$FAKE/config.tar" "$FAKE/pod/home/task.bundle"; do lacks "$f" TOPSECRET; done
adapter start --kind agent --bundle "$T/missing.bundle" --dir "$DIR" && fail "a missing bundle was accepted"
has "$T/err" 'does not exist'
fresh
adapter start --kind agent --dir "$DIR" || fail "$(cat "$T/err")"
[ ! -e "$FAKE/pod/root/a.txt" ] || fail "a start without a bundle cloned something"
[ ! -e "$FAKE/pod/home/task.bundle" ] || fail "a start without a bundle uploaded one"

# list: each pod's agents with a kind (agent when mu did not say); connect finds an agent's pod by its id.
fresh
printf 'mu-agent-1|agent|x|Running\nw9|worker|x|Running\n' > "$FAKE/pods.tsv"
echo '{"id":"a1","cwd":"/p"}' > "$FAKE/list-mu-agent-1"
echo '{"id":"w9","kind":"worker"}' > "$FAKE/list-w9"
adapter list || fail "list failed: $(cat "$T/err")"
has "$T/out" '{"id":"a1","cwd":"/p","kind":"agent"}'
has "$T/out" '{"id":"w9","kind":"worker"}'
adapter connect a1 || true
has "$FAKE/calls" 'exec -i mu-agent-1 -- mu-remote bridge a1'
adapter connect nope && fail "connect to an unknown id succeeded"
has "$T/err" 'no pod runs nope'

# read and write: files through kubectl exec; write creates parents and never overwrites.
fresh
printf 'mu-agent-1|agent|x|Running\n' > "$FAKE/pods.tsv"
echo '{"id":"a1","cwd":"/p"}' > "$FAKE/list-mu-agent-1"
mkdir -p "$FAKE/pod/root"
printf 'file-bytes' > "$FAKE/pod/root/x"
adapter read a1 "$DIR/x" || fail "read failed: $(cat "$T/err")"
[ "$(cat "$T/out")" = file-bytes ] || fail "read printed: $(cat "$T/out")"
has "$FAKE/execs" "mu-agent-1 cat -- $FAKE/pod/root/x"
has "$FAKE/calls" "exec mu-agent-1 -- cat -- $DIR/x"
adapter read a1 "$DIR/missing" && fail "read of a missing file succeeded"
printf 'uploaded' | adapter write a1 "$DIR/new/up.txt" || fail "write failed: $(cat "$T/err")"
[ "$(cat "$FAKE/pod/root/new/up.txt")" = uploaded ] || fail "write did not create the file"
has "$FAKE/calls" "exec -i mu-agent-1 -- sh -c umask 077; mkdir -p -- \"\$(dirname -- \"\$1\")\" && [ ! -e \"\$1\" ] && cat > \"\$1\" sh $DIR/new/up.txt"
printf 'again' | adapter write a1 "$DIR/new/up.txt" && fail "write replaced a file"
[ "$(cat "$FAKE/pod/root/new/up.txt")" = uploaded ] || fail "a refused write changed the file"
adapter read nope "$DIR/x" && fail "read from an unknown id succeeded"
has "$T/err" 'no pod runs nope'

# forward and unforward: kubectl port-forward on 127.0.0.1, recorded by pid; unforward ends it.
adapter forward a1 3000 4001 || fail "forward failed: $(cat "$T/err")"
[ -z "$(cat "$T/out")" ] || fail "forward printed: $(cat "$T/out")"
has "$FAKE/calls" 'port-forward --address 127.0.0.1 pod/mu-agent-1 4001:3000'
pidfile=$T/state/forwards/mu-agent-1-4001.pid
[ -s "$pidfile" ] || fail "no pid file"
fpid=$(cat "$pidfile")
kill -0 "$fpid" || fail "the port-forward is not running"
adapter unforward a1 3000 4001 || fail "unforward failed: $(cat "$T/err")"
[ ! -e "$pidfile" ] || fail "unforward left the pid file"
# Gone, or a zombie that nothing has reaped yet (a container's init may not).
alive() { [ -n "$(ps -o stat= -p "$1" 2> /dev/null | grep -v '^Z')" ]; }
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  alive "$fpid" || break
  sleep 0.1
done
alive "$fpid" && fail "unforward did not kill pid $fpid"
adapter unforward a1 3000 4001 || fail "unforward without a pid file failed: $(cat "$T/err")"
touch "$FAKE/forward-fails"
adapter forward a1 3000 4002 && fail "a failed port-forward succeeded"
has "$T/err" 'address already in use'
[ "$(wc -l < "$T/err" | tr -d ' ')" = 1 ] || fail "the failure is not one line: $(cat "$T/err")"
[ ! -e "$T/state/forwards/mu-agent-1-4002.pid" ] || fail "a failed forward left a pid file"

# Sweep: only this launcher's ended pods.
fresh
printf 'mine-done|worker|%s|Succeeded\nmine-failed|worker|%s|Failed\nmine-live|worker|%s|Running\nother-done|worker|elsewhere|Succeeded\n' \
  "$id1" "$id1" "$id1" > "$FAKE/pods.tsv"
mk_bundle w7
adapter start --kind worker --launch w7 --bundle "$T/w7.bundle" --dir "$DIR" || fail "$(cat "$T/err")"
has "$FAKE/deletes" mine-done
has "$FAKE/deletes" mine-failed
lacks "$FAKE/deletes" mine-live
lacks "$FAKE/deletes" other-done
[ "$(cat "$T/state/launcher-id")" = "$id1" ] || fail "the launcher id changed"

adapter bogus && fail "an unknown verb succeeded"
has "$T/err" 'unknown verb'

echo "test-pod-run: ok"
