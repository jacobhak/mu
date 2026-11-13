#!/usr/bin/env bash
# Native-provider check, offline: the TUI sends a prompt to a loopback fake of the Anthropic Messages
# API through a key stored in mu's key store, and a fake `omp` records any call (a native session
# must make none). Opt-in, like smoke.sh; not part of `cargo test`.
# Usage: scripts/smoke-native.sh [path-to-mu]   Needs: tmux, python3.
set -uo pipefail
mu="$(readlink -f "${1:-target/release/mu}")"
MU_HOME="$(mktemp -d)"
export MU_HOME
export OMP_PROFILE="mutest-$$"
work="$(mktemp -d)"
tm="mu-native-smoke-$$"
fail=0
srv_pid=""
pass() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }
cleanup() {
  tmux kill-session -t "$tm" 2>/dev/null
  [ -n "$srv_pid" ] && kill "$srv_pid" 2>/dev/null
  rm -rf "$MU_HOME" "$work"
}
trap cleanup EXIT

screen() { tmux capture-pane -pt "$tm" -S -200; }
wait_for() { # pattern timeout_s
  local end=$((SECONDS + ${2:-30}))
  while ((SECONDS < end)); do screen | grep -qF -- "$1" && return 0; sleep 0.5; done
  return 1
}
check() { # name command...
  local name="$1"; shift
  if "$@"; then pass "$name"; else bad "$name"; fi
}

# Fake Messages API: answers POST /v1/messages with a text stream, logs method, path and key presence.
cat > "$work/fake_anthropic.py" <<'PY'
import http.server, json, sys

LOG, PORT_FILE = sys.argv[1], sys.argv[2]
EVENTS = [
    ("message_start", {"type": "message_start", "message": {
        "id": "msg_smoke", "type": "message", "role": "assistant", "model": "claude-haiku-5-5",
        "content": [], "stop_reason": None, "stop_sequence": None,
        "usage": {"input_tokens": 8, "output_tokens": 1}}}),
    ("content_block_start", {"type": "content_block_start", "index": 0,
                             "content_block": {"type": "text", "text": ""}}),
    ("content_block_delta", {"type": "content_block_delta", "index": 0,
                             "delta": {"type": "text_delta", "text": "NATIVE-OK"}}),
    ("content_block_stop", {"type": "content_block_stop", "index": 0}),
    ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                       "usage": {"output_tokens": 3}}),
    ("message_stop", {"type": "message_stop"}),
]
BODY = "".join(f"event: {e}\ndata: {json.dumps(d)}\n\n" for e, d in EVENTS).encode()

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        has_key = "yes" if self.headers.get("x-api-key") else "no"
        with open(LOG, "a") as f:
            f.write(f"{self.command} {self.path} x-api-key={has_key}\n")
        if self.path != "/v1/messages":
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)

    def log_message(self, *args):
        pass

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(PORT_FILE, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
PY
python3 "$work/fake_anthropic.py" "$work/requests.log" "$work/port" &
srv_pid=$!
for _ in $(seq 40); do [ -s "$work/port" ] && break; sleep 0.25; done
port="$(cat "$work/port" 2>/dev/null)"
check "fake API listening" test -n "$port"

# A fake omp: every call is recorded and fails.
mkdir -p "$work/bin"
printf '#!/bin/sh\necho "$0 $*" >> "%s/omp-calls.log"\nexit 1\n' "$work" > "$work/bin/omp"
chmod +x "$work/bin/omp"

cd "$work" || exit 1
key_marker="sk-smoke-native-key"
printf '%s' "$key_marker" | env -u ANTHROPIC_API_KEY MU_OMP="$work/bin/omp" \
  "$mu" auth add anthropic --base-url "http://127.0.0.1:$port" --no-validate >/dev/null 2>&1
key_listed() { env -u ANTHROPIC_API_KEY "$mu" auth list | grep -q anthropic; }
check "key stored for the fake API" key_listed

tmux new -d -s "$tm" -x 150 -y 40 \
  "cd $work && env -u ANTHROPIC_API_KEY MU_OMP=$work/bin/omp MU_HOME=$MU_HOME OMP_PROFILE=$OMP_PROFILE $mu --model anthropic/claude-haiku-5-5; echo EXIT=\$?; sleep 60"
wait_for "μ" 20 || bad "tui starts"
tmux send-keys -t "$tm" 'Say hi.' Enter
check "native reply renders" wait_for "NATIVE-OK" 30
check "the request reached the fake API with a key" grep -q "POST /v1/messages x-api-key=yes" "$work/requests.log"
check "the key is not echoed on screen" test -z "$(screen | grep -F "$key_marker")"
check "no omp process was spawned" test ! -e "$work/omp-calls.log"

tmux send-keys -t "$tm" C-c
tmux send-keys -t "$tm" C-c
check "double Ctrl-C exits 0" wait_for "EXIT=0" 20

exit $fail
