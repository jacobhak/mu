//! The launcher end to end: the real `mu` runs a task through `[task] launcher = mu-remote launch`, a fake
//! adapter runs the worker on this machine with a bare home (no key, no gateway), and a local fake API plays
//! the model.
#![allow(clippy::unwrap_used)]

mod common;

use common::{MODEL, home_with_key, mu, repo, sh, tmp};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MU_REMOTE: &str = env!("CARGO_BIN_EXE_mu-remote");

fn text_reply(text: &str) -> String {
    common::ANSWER.replace("hello from the fake", text)
}

fn tool_use_body(id: &str, name: &str, input: &Value) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m1\"}}}}\n\n\
         event: content_block_start\ndata: {}\n\n\
         event: content_block_delta\ndata: {}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":2}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
    )
}

type Seen = Arc<Mutex<Vec<String>>>;

/// A fake model that routes on the request body, since mu also sends title requests whose timing is not
/// fixed: a title; the parent (its history holds "delegate it"): a `task` call, then, once it has the result,
/// a last answer; the child: a `write` of `child.txt`, then, once it has the tool result, `child_says`.
fn relay_model(child_says: &str) -> (String, Seen) {
    let child_says = child_says.to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let call =
        json!({"tasks": [{"name": "Relayed", "task": "say the word", "agent": "task", "solutionSpace": "one word"}]});
    let write = json!({"path": "child.txt", "content": "from the child\n"});
    let seen = Seen::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let (call, write, child_says, seen) = (call.clone(), write.clone(), child_says.clone(), log.clone());
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = sock.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    let Some(p) = text.find("\r\n\r\n") else {
                        if n == 0 {
                            return;
                        }
                        continue;
                    };
                    let len = text[..p]
                        .to_lowercase()
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    if buf.len() >= p + 4 + len || n == 0 {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let body = if req.contains("3–7 word title") {
                    text_reply("A title")
                } else if req.contains("delegate it") {
                    if req.contains("\"tool_result\"") {
                        text_reply("parent finished")
                    } else {
                        tool_use_body("t1", "task", &call)
                    }
                } else if req.contains("\"tool_result\"") {
                    text_reply(&child_says)
                } else {
                    tool_use_body("w1", "write", &write)
                };
                seen.lock().unwrap().push(req);
                let out = format!(
                    "HTTP/1.1 200 X\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(out.as_bytes());
            });
        }
    });
    (base, seen)
}

/// A parent home pointed at `relay_model`, a git cwd and the requests the server saw.
fn fixture() -> (PathBuf, PathBuf, Seen) {
    let (base, seen) = relay_model("relayed child result");
    let home = home_with_key(&base);
    std::fs::write(home.join("models.toml"), format!("[sets.main]\ndefault = \"{MODEL}\"\n")).unwrap();
    (home, repo(), seen)
}

/// The requests the child sent: they carry the task text and no parent history.
fn child_requests(seen: &[String]) -> Vec<&String> {
    seen.iter()
        .filter(|r| r.contains("say the word") && !r.contains("delegate it") && !r.contains("3–7 word title"))
        .collect()
}

fn on_path(program: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join(program))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{program} on PATH"))
}

/// How a fake remote behaves.
#[derive(Default)]
struct Behaviour {
    trusted: bool,
    /// `repos` of an untrusted remote.
    repos: Vec<PathBuf>,
    /// The first connection is cut after this many lines from the worker (0: never). The first of them is
    /// the handshake, the rest are encrypted.
    cut_first: usize,
}

/// A remote that runs its workers on this machine, with a bare home, through an adapter script of the
/// contract that uses `mu-remote worker …` and `mu-remote bridge`. Everything the adapter receives is logged.
struct Remote {
    bare: PathBuf,
    log: PathBuf,
    /// `mu-remote`'s config file with `[remote.fake]`.
    config: PathBuf,
}

impl Drop for Remote {
    fn drop(&mut self) {
        for e in std::fs::read_dir(self.bare.join("state/workers")).into_iter().flatten().flatten() {
            let state = std::fs::read_to_string(e.path().join("state.json")).unwrap_or_default();
            if let Some(pid) = serde_json::from_str::<Value>(&state).ok().and_then(|v| v["pid"].as_i64()) {
                // SAFETY: a plain signal to a process this test started.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            }
        }
    }
}

impl Remote {
    /// The adapter's received environment, stdin of every connection, stdout of every connection and task
    /// bundles, as one text.
    fn received(&self) -> String {
        let mut all = Vec::new();
        for e in std::fs::read_dir(&self.log).unwrap().flatten() {
            all.extend(std::fs::read(e.path()).unwrap());
        }
        String::from_utf8_lossy(&all).into_owned()
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.log.join(name)).unwrap_or_default()
    }
}

/// Makes `[remote.fake]` over a fake adapter. The worker's `mu` is `worker_mu`.
fn fake_remote(dir: &Path, worker_mu: &Path, b: Behaviour) -> Remote {
    let bare = tmp();
    std::fs::create_dir_all(bare.join("empty-bin")).unwrap();
    let _ = std::fs::copy(dir.join("models.toml"), bare.join("models.toml"));
    let log = tmp();
    let script = format!(
        r#"#!/bin/sh
PATH='{path}'; export PATH
unset MU_GATEWAY_URL ANTHROPIC_API_KEY
env > '{log}'/env.$1.$$
for v in $(env | grep -o '^MU_TASK_[A-Z_]*'); do unset "$v"; done
W='{bare}'
as_worker() {{ MU_REMOTE_HOME="$W/state" MU_HOME="$W" HOME="$W" MU_TEST_HOME="$W" MU_REMOTE_MU='{mu}' '{bin}' "$@"; }}
case "$1" in
  version) echo '{{"contract":1}}' ;;
  start)
    shift
    while [ $# -gt 0 ]; do
      case "$1" in --launch) launch=$2 ;; --bundle) bundle=$2 ;; esac
      shift
    done
    cat "$bundle" >> '{log}'/bundle.bin
    d="$W/work/$launch"
    git init -q "$d" && git -C "$d" fetch -q "$bundle" "refs/mu/task/$launch:refs/mu/task/$launch" \
      && git -C "$d" checkout -q --detach "refs/mu/task/$launch" || exit 1
    echo '{{"progress":"worker checkout ready"}}' >&2
    cd "$d" && as_worker worker run --launch "$launch" --dir "$d" --detach < /dev/null
    ;;
  connect)
    n=$(cat '{log}'/connects.n 2>/dev/null || echo 0)
    echo $((n + 1)) > '{log}'/connects.n
    if [ "$n" -eq 0 ] && [ {cut} -gt 0 ]; then
      {{ tee -a '{log}'/to-worker | as_worker bridge "$2"; }} \
        | {{ k=0; while [ $k -lt {cut} ] && IFS= read -r l; do printf '%s\n' "$l" | tee -a '{log}'/from-worker; k=$((k + 1)); done; kill $$; }}
    else
      tee -a '{log}'/to-worker | as_worker bridge "$2" | tee -a '{log}'/from-worker
    fi
    ;;
  fetch) as_worker worker bundle "$2" ;;
  stop) as_worker worker stop "$2" ;;
  *) echo "unknown verb $1" >&2; exit 3 ;;
esac
"#,
        path = std::env::var("PATH").unwrap(),
        log = log.display(),
        bare = bare.display(),
        mu = worker_mu.display(),
        bin = MU_REMOTE,
        cut = b.cut_first,
    );
    let adapter = dir.join("adapter.sh");
    std::fs::write(&adapter, script).unwrap();
    std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repos = b.repos.iter().map(|r| format!("{:?}", r.display().to_string())).collect::<Vec<_>>().join(", ");
    let config = dir.join("mu-remote.toml");
    std::fs::write(
        &config,
        format!(
            "[remote.fake]\nadapter = {:?}\ntrusted = {}\nrepos = [{repos}]\nreconnect_timeout = 60\n",
            adapter.display().to_string(),
            b.trusted
        ),
    )
    .unwrap();
    Remote { bare, log, config }
}

/// The parent's config: tasks launch through `mu-remote launch fake`. Returns the `mu` command with
/// everything the launcher needs in its environment.
fn parent(home: &Path, cwd: &Path, remote: &Remote) -> Command {
    // The parent's PATH is empty, but launching a task commits the tree with git.
    let _ = std::os::unix::fs::symlink(on_path("git"), home.join("empty-bin/git"));
    std::fs::write(
        home.join("config.toml"),
        format!("[task]\nlaunch = [\"task\"]\nlauncher = [{MU_REMOTE:?}, \"launch\", \"fake\"]\n"),
    )
    .unwrap();
    let mut c = mu(home, cwd);
    c.env("MU_REMOTE_CONFIG", &remote.config).env("MU_REMOTE_HOME", home.join("remote-state"));
    c
}

fn run_parent(c: &mut Command) -> std::process::Output {
    c.args(["-p", "delegate it", "--model", MODEL, "--permissions", "yolo"]).output().unwrap()
}

fn stderr(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn parent_got_result(seen: &Seen) -> bool {
    seen.lock().unwrap().iter().any(|r| r.contains("\"tool_result\"") && r.contains("relayed child result"))
}

/// A launched child has no credentials and no gateway: its model calls reach the server only through the
/// parent, and its changes come back as a bundle that mu applies.
#[test]
fn a_launched_child_with_no_credentials_reaches_the_model_through_the_parent() {
    let mu_bin = need_mu!();
    let (home, cwd, seen) = fixture();
    let remote = fake_remote(&home, &mu_bin, Behaviour { trusted: true, ..Default::default() });
    let o = run_parent(&mut parent(&home, &cwd, &remote));
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let requests = seen.lock().unwrap();
    assert!(!child_requests(&requests).is_empty(), "the server saw the child's request: {requests:?}");
    assert!(
        requests.iter().any(|r| r.contains("\"tool_result\"") && r.contains("relayed child result")),
        "the child's result reaches the parent's next request"
    );
    assert!(requests.iter().any(|r| r.contains("[changes: applied")), "the result bundle was applied: {requests:?}");
    assert_eq!(std::fs::read_to_string(cwd.join("child.txt")).unwrap(), "from the child\n");
    assert!(stdout(&o).contains("parent finished"), "stdout: {}", stdout(&o));
    assert!(!remote.bare.join("auth.json").exists(), "the child never held a key store");
    let stopped = std::fs::read_dir(remote.bare.join("state/workers")).unwrap().count();
    assert_eq!(stopped, 1, "one worker ran");
}

/// The stream to the worker is cut after a few lines, with the child's model call in flight. The launcher
/// reconnects, both sides replay what the other lacks, and the task finishes with its model called once.
#[test]
fn a_worker_stream_cut_with_a_relay_call_in_flight_is_resumed_and_the_task_completes() {
    let mu_bin = need_mu!();
    let (home, cwd, seen) = fixture();
    let remote = fake_remote(&home, &mu_bin, Behaviour { trusted: true, cut_first: 5, ..Default::default() });
    let o = run_parent(&mut parent(&home, &cwd, &remote));
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(remote.log("connects.n").trim().parse::<u32>().unwrap() >= 2, "it reconnected");
    let requests = seen.lock().unwrap();
    let child = child_requests(&requests);
    assert_eq!(
        child.iter().filter(|r| !r.contains("\"tool_result\"")).count(),
        1,
        "the worker's model call ran once: {requests:?}"
    );
    assert_eq!(child.len(), 2, "its call and the one after its tool: {requests:?}");
    assert!(requests.iter().any(|r| r.contains("[changes: applied")), "the result is complete: {requests:?}");
    assert!(stdout(&o).contains("parent finished"), "stdout: {}", stdout(&o));
    // Both connections were encrypted: the stream carries no line the launcher could read.
    let from = remote.log("from-worker");
    assert!(from.lines().count() > 2 && !from.contains("\"seq\"") && !from.contains("\"type\""), "{from}");
    let to = remote.log("to-worker");
    assert!(!to.is_empty() && !to.contains("\"type\"") && !to.contains("delegate"), "{to}");
}

/// A worker for an untrusted remote gets nothing secret from the parent: not in the adapter's environment,
/// the repository bundle, the task payload or any line of the stream either way.
#[test]
fn a_worker_on_an_untrusted_remote_is_sent_no_secret() {
    const SECRET: &str = "sk-test-key";
    let mu_bin = need_mu!();
    let (home, cwd, seen) = fixture();
    let remote = fake_remote(&home, &mu_bin, Behaviour { repos: vec![cwd.clone()], ..Default::default() });
    let mut c = parent(&home, &cwd, &remote);
    c.env("SEEDED_API_KEY", SECRET).env("SEEDED_SERVICE_TOKEN", SECRET);
    let o = run_parent(&mut c);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(std::fs::read_to_string(home.join("auth.json")).unwrap().contains(SECRET), "the parent holds it");
    assert!(parent_got_result(&seen), "the task ran");
    let envs: Vec<String> = std::fs::read_dir(&remote.log)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("env."))
        .collect();
    assert!(envs.len() >= 4, "every verb ran: {envs:?}");
    let to = remote.log("to-worker");
    assert!(!to.is_empty() && !to.contains("\"type\"") && !to.contains("delegate"), "the payload is encrypted: {to}");
    assert!(
        !remote.log("from-worker").is_empty() && std::fs::metadata(remote.log.join("bundle.bin")).unwrap().len() > 0
    );
    let all = remote.received();
    assert!(!all.contains(SECRET), "a secret reached the adapter");
    assert!(!all.contains("SEEDED_"), "even the names of the variables were scrubbed");
}

/// An untrusted remote runs only the repositories it lists, and only while what would be sent holds no
/// credential-looking file.
#[test]
fn an_untrusted_remote_refuses_an_unlisted_repository_and_a_config_with_credentials() {
    let mu_bin = need_mu!();
    let (home, cwd, seen) = fixture();
    let remote = fake_remote(&home, &mu_bin, Behaviour::default());
    let o = run_parent(&mut parent(&home, &cwd, &remote));
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    {
        let requests = seen.lock().unwrap();
        assert!(child_requests(&requests).is_empty(), "no worker ran: {requests:?}");
        assert!(
            requests.iter().any(|r| r.contains("\"tool_result\"") && r.contains("remote fake is not trusted for")),
            "{requests:?}"
        );
    }
    assert!(remote.received().is_empty(), "the adapter was never run");

    let (home, cwd, seen) = fixture();
    let remote = fake_remote(&home, &mu_bin, Behaviour { repos: vec![cwd.clone()], ..Default::default() });
    let agents = home.join(".omp/agent/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join(".env"), "TOKEN=1\n").unwrap();
    let o = run_parent(&mut parent(&home, &cwd, &remote));
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let requests = seen.lock().unwrap();
    assert!(child_requests(&requests).is_empty(), "{requests:?}");
    assert!(
        requests.iter().any(|r| r.contains("\"tool_result\"") && r.contains("credential-looking files")),
        "{requests:?}"
    );
    assert!(remote.received().is_empty(), "the adapter was never run");
}

/// A worker's `mu`: reads the payload, makes a change, reports a tool call and then waits.
fn waiting_mu(dir: &Path) -> PathBuf {
    let path = dir.join("fake-mu");
    std::fs::write(
        &path,
        "#!/bin/sh\nIFS= read -r payload\necho changed > made.txt\n\
         echo '{\"type\":\"tool\",\"name\":\"t\",\"summary\":\"working\"}'\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Cancel is TERM to the launcher: it stops the worker, writes the result it has and exits.
#[test]
fn terminating_the_launcher_stops_the_worker_writes_a_result_and_exits() {
    let dir = tmp();
    let worker_mu = waiting_mu(&dir);
    let remote = fake_remote(&dir, &worker_mu, Behaviour { trusted: true, ..Default::default() });
    let root = repo();
    let id = "cancelme-0a1b2c3d";
    let task_ref = format!("refs/mu/task/{id}");
    sh(&root, &["update-ref", &task_ref, "HEAD"]);
    let bundle = dir.join("task.bundle");
    sh(&root, &["bundle", "create", "-q", &bundle.to_string_lossy(), &task_ref]);
    let result = dir.join("result.bundle");
    std::fs::write(&result, "").unwrap();
    let mut child = Command::new(MU_REMOTE)
        .args(["launch", "fake"])
        .current_dir(&root)
        .env("MU_REMOTE_CONFIG", &remote.config)
        .env("MU_REMOTE_HOME", dir.join("launcher-state"))
        .env("MU_TASK_CONTRACT", "2")
        .env("MU_TASK_NAME", id)
        .env("MU_TASK_ROOT", &root)
        .env("MU_TASK_CWD", &root)
        .env("MU_TASK_REF", &task_ref)
        .env("MU_TASK_DEPTH", "1")
        .env("MU_TASK_AGENT_DIR", dir.join("agent"))
        .env("MU_TASK_BUNDLE", &bundle)
        .env("MU_TASK_RESULT_BUNDLE", &result)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"task": "wait", "depth": 1})).unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    out.read_line(&mut first).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap(),
        json!({"type": "tool", "name": "t", "summary": "working"})
    );

    let started = Instant::now();
    // SAFETY: a plain signal to the launcher this test started.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    let heads = sh(&root, &["bundle", "list-heads", &result.to_string_lossy()]);
    assert!(heads.contains(&format!("refs/mu/result/{id}")), "the result holds what the worker had: {heads:?}");
    let mut rest = String::new();
    out.read_to_string(&mut rest).unwrap();
    assert!(rest.is_empty(), "nothing but the child's lines goes to stdout: {rest:?}");
    // The worker is gone.
    let state = std::fs::read_to_string(remote.bare.join(format!("state/workers/{id}/state.json"))).unwrap();
    let pid = serde_json::from_str::<Value>(&state).unwrap()["pid"].as_i64().unwrap() as i32;
    // SAFETY: signal 0 only asks whether the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default().contains(") Z");
    assert!(!alive, "the worker process ended");
}
