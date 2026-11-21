//! Smoke tests of each front-end mode: the real binary, a private `MU_HOME` and cwd, no omp, and
//! an Anthropic API served by a local fake. Nothing here touches the network or the real `~/.mu`.
use mu_testutil::{Reply, block_on, fake_server};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;

const MODEL: &str = "anthropic/claude-sonnet-5-5";

const ANSWER: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\n\
    event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
    event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello from the fake\"}}\n\n\
    event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
    event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}\n\n\
    event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

/// Serves `replies` in order from a background thread (the fake's task needs a live runtime).
/// Returns its base URL.
fn serve(replies: Vec<Reply>) -> String {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        block_on(async move {
            let (base, _) = fake_server(replies).await;
            tx.send(base).unwrap();
            std::future::pending::<()>().await;
        })
    });
    rx.recv().unwrap()
}

fn sse(body: &str) -> Reply {
    Reply::new(200, body).header("content-type", "text/event-stream")
}

/// A `MU_HOME` (also `HOME`) whose key store holds an Anthropic key that points at `base`.
fn home_with_key(base: &str) -> mu_testutil::TempDir {
    let home = mu_testutil::tmp();
    let store = json!({"version": 1, "providers": {"anthropic": {"key": "sk-test-key", "baseUrl": base}}});
    std::fs::write(home.join("auth.json"), store.to_string()).unwrap();
    std::fs::create_dir_all(home.join("empty-bin")).unwrap();
    home
}

/// The binary with a private home and cwd, and an empty `PATH` so no host program is found.
fn mu(home: &Path, cwd: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mu"));
    c.current_dir(cwd)
        .env("MU_TEST_HOME", home)
        .env("MU_HOME", home)
        .env("HOME", home)
        .env("PATH", home.join("empty-bin"))
        .env_remove("OMP_PROFILE")
        .env_remove("MU_GATEWAY_URL")
        .env_remove("MU_THEME")
        .env_remove("ANTHROPIC_API_KEY");
    c
}

fn run_with_stdin(c: &mut Command, stdin: &str) -> Output {
    let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn print_mode_prints_the_final_answer_and_exits_zero() {
    let home = home_with_key(&serve(vec![sse(ANSWER)]));
    let cwd = mu_testutil::tmp();
    let o = mu(&home, &cwd).args(["-p", "hello", "--model", MODEL]).output().unwrap();
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert_eq!(stdout(&o), "hello from the fake\n");
}

#[test]
fn print_mode_with_a_rejected_key_exits_nonzero_with_the_error() {
    let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let home = home_with_key(&serve(vec![Reply::new(401, body)]));
    let cwd = mu_testutil::tmp();
    let o = mu(&home, &cwd).args(["-p", "hello", "--model", MODEL]).output().unwrap();
    assert!(!o.status.success());
    assert!(stderr(&o).contains("invalid x-api-key"), "stderr: {}", stderr(&o));
}

#[test]
fn headless_runs_the_task_payload_and_reports_its_result() {
    let home = home_with_key(&serve(vec![sse(ANSWER)]));
    let cwd = mu_testutil::tmp();
    let payload = json!({"agent": "task", "task": "say hello"}).to_string();
    let mut c = mu(&home, &cwd);
    let o = run_with_stdin(c.args(["--headless", "--model", MODEL]), &payload);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let lines: Vec<serde_json::Value> = stdout(&o).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let result = lines.last().unwrap();
    assert_eq!(result["type"], "result", "{lines:?}");
    assert_eq!(result["text"], "hello from the fake");
    assert_eq!(result["isError"], false);
}

/// A scout payload carries no agent file. Without a built-in one it ran the main agent's prompt and
/// was told to use a shell it does not have.
#[test]
fn a_scout_gets_its_own_prompt_and_no_shell() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        block_on(async move {
            tx.send(fake_server(vec![sse(ANSWER)]).await).unwrap();
            std::future::pending::<()>().await;
        })
    });
    let (base, seen) = rx.recv().unwrap();
    let home = home_with_key(&base);
    let cwd = mu_testutil::tmp();
    let payload = json!({"agent": "scout", "task": "say hello"}).to_string();
    let mut c = mu(&home, &cwd);
    let o = run_with_stdin(c.args(["--headless", "--model", MODEL]), &payload);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let result: serde_json::Value = stdout(&o).lines().map(|l| serde_json::from_str(l).unwrap()).next_back().unwrap();
    assert_eq!(result["text"], "hello from the fake");
    let requests = seen.lock().unwrap();
    let body = requests.first().expect("a request");
    assert!(body.contains("This run has no shell"), "the scout prompt: {body}");
    assert!(!body.contains("MUST use `bash`"), "no shell rules for a scout: {body}");
    assert!(body.contains("\"name\":\"read\""), "read is offered: {body}");
    assert!(!body.contains("\"name\":\"bash\""), "bash is not offered: {body}");
}

/// Nobody can approve `enter_plan` in a print run or a subagent, and a subagent cannot plan at all.
#[test]
fn print_and_headless_runs_are_offered_neither_plan_tool() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        block_on(async move {
            tx.send(fake_server(vec![sse(ANSWER), sse(ANSWER)]).await).unwrap();
            std::future::pending::<()>().await;
        })
    });
    let (base, seen) = rx.recv().unwrap();
    let home = home_with_key(&base);
    let cwd = mu_testutil::tmp();
    let o = mu(&home, &cwd).args(["-p", "hello", "--model", MODEL]).output().unwrap();
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let payload = json!({"agent": "task", "task": "say hello"}).to_string();
    let mut c = mu(&home, &cwd);
    let o = run_with_stdin(c.args(["--headless", "--model", MODEL]), &payload);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for body in requests.iter() {
        assert!(body.contains("\"name\":\"read\""), "tools are offered: {body}");
        assert!(!body.contains("\"name\":\"enter_plan\"") && !body.contains("\"name\":\"propose_plan\""), "{body}");
    }
}

/// Every line of every session file under `dir`.
fn session_texts(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
            }
        }
    }
    out
}

/// `[task] soft_request_budget` reaches the child: a scout on a budget of 2 is steered to wrap up
/// on its second request, and the notice is in its own session.
#[test]
fn a_subagent_runs_under_the_configured_request_budget() {
    let replies = vec![
        tool_use("t1", "read", json!({"path": "a.txt"})),
        tool_use("t2", "read", json!({"path": "a.txt"})),
        tool_use("t3", "read", json!({"path": "a.txt"})),
        sse(ANSWER),
    ];
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        block_on(async move {
            tx.send(fake_server(replies).await).unwrap();
            std::future::pending::<()>().await;
        })
    });
    let (base, _seen) = rx.recv().unwrap();
    let home = home_with_key(&base);
    std::fs::write(home.join("config.toml"), "[task]\nsoft_request_budget = 2\n").unwrap();
    let cwd = mu_testutil::tmp();
    std::fs::write(cwd.join("a.txt"), "x\n").unwrap();
    let payload = json!({"agent": "scout", "task": "look"}).to_string();
    let mut c = mu(&home, &cwd);
    let o = run_with_stdin(c.args(["--headless", "--model", MODEL]), &payload);
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let sessions = session_texts(&home.join("sessions"));
    assert!(sessions.contains("[budget notice]"), "the wrap-up notice is in the child session");
    assert!(sessions.contains("soft budget: 2"), "the configured budget, not the bundled 100");
    assert!(sessions.contains("no more tools will run"), "the stop follows");
}

#[test]
fn plan_mode_without_a_parked_plan_exits_nonzero() {
    // The fake answers with text only, so the plan is never proposed and parked.
    let home = home_with_key(&serve(vec![sse(ANSWER)]));
    let cwd = mu_testutil::repo();
    let o = mu(&home, &cwd).args(["--model", MODEL, "plan", "new", "add a greeting"]).output().unwrap();
    assert!(!o.status.success());
    assert!(stderr(&o).contains("no plan was parked"), "stderr: {}", stderr(&o));
}

fn tool_use(id: &str, name: &str, input: serde_json::Value) -> Reply {
    sse(&tool_use_body(id, name, &input))
}

fn tool_use_body(id: &str, name: &str, input: &serde_json::Value) -> String {
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

#[test]
fn the_model_can_search_mu_docs_with_grep() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        block_on(async move {
            let replies = vec![
                tool_use("t1", "grep", json!({"pattern": "migrate-profile", "path": "mu://reference"})),
                sse(ANSWER),
            ];
            tx.send(fake_server(replies).await).unwrap();
            std::future::pending::<()>().await;
        })
    });
    let (base, seen) = rx.recv().unwrap();
    let home = home_with_key(&base);
    let cwd = mu_testutil::tmp();
    let o = mu(&home, &cwd).args(["-p", "how do I move a profile?", "--model", MODEL]).output().unwrap();
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let requests = seen.lock().unwrap();
    let second = requests.get(1).expect("a second request carries the tool result");
    assert!(second.contains("mu://reference/cli.md:"), "{second}");
    assert!(home.join("docs").is_dir(), "the docs were written under MU_HOME");
}

fn text_reply(text: &str) -> String {
    ANSWER.replace("hello from the fake", text)
}

/// A fake model that routes on the request body, since mu also sends title requests whose timing
/// is not fixed: a title, the parent's first call (answers with a `task` call), the parent's call
/// after the task result, and otherwise the child's.
fn relay_model(child_says: &str) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    relay_model_spending(child_says, 10)
}

/// `relay_model` whose child's replies report `child_input_tokens` of input.
fn relay_model_spending(
    child_says: &str,
    child_input_tokens: u64,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::Read;
    let child_says = child_says.to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let call =
        json!({"tasks": [{"name": "Relayed", "task": "say the word", "agent": "task", "solutionSpace": "one word"}]});
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let (call, child_says, seen) = (call.clone(), child_says.clone(), log.clone());
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
                } else if req.contains("\"tool_result\"") {
                    text_reply("parent finished")
                } else if req.contains("delegate it") {
                    tool_use_body("t1", "task", &call)
                } else {
                    text_reply(&child_says)
                        .replace("\"input_tokens\":10", &format!("\"input_tokens\":{child_input_tokens}"))
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
fn relay_fixture(
    child_says: &str,
) -> (mu_testutil::TempDir, mu_testutil::TempDir, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    relay_fixture_from(relay_model(child_says))
}

fn relay_fixture_from(
    (base, seen): (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>),
) -> (mu_testutil::TempDir, mu_testutil::TempDir, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let home = home_with_key(&base);
    std::fs::write(home.join("models.toml"), format!("[sets.main]\ndefault = \"{MODEL}\"\n")).unwrap();
    (home, mu_testutil::repo(), seen)
}

/// What the server saw split by who sent it: the child's requests carry the task text and no
/// parent prompt.
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

/// What a test launcher leaves as its result.
#[derive(Clone, Copy)]
enum Leaves {
    /// Nothing changed: an empty result bundle.
    Nothing,
    /// `a.txt` changed, in the bundle contract A names.
    Changed,
    /// A bundle holding a ref the launch does not own.
    Forged,
}

/// A launcher (launcher contract 2) that runs the subagent on this machine with a bare home (no key,
/// no gateway), the way a remote's would. Its environment is logged in `log`.
struct Launcher {
    bare: mu_testutil::TempDir,
    log: mu_testutil::TempDir,
}

impl Launcher {
    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.log.join(name)).unwrap_or_default()
    }
}

/// Sets `[task] launcher` and `launch = ["task"]` in the parent's `home` over a scripted launcher.
fn launcher_for(home: &Path, result: Leaves) -> Launcher {
    use std::os::unix::fs::PermissionsExt;
    let bare = mu_testutil::tmp();
    std::fs::create_dir_all(bare.join("empty-bin")).unwrap();
    std::fs::copy(home.join("models.toml"), bare.join("models.toml")).unwrap();
    let log = mu_testutil::tmp();
    // The parent's PATH is empty, but launching a task commits the tree with git.
    let _ = std::os::unix::fs::symlink(on_path("git"), home.join("empty-bin/git"));
    let (ref_name, with_base) = match result {
        Leaves::Forged => ("refs/heads/forged", ""),
        _ => ("refs/mu/result/$MU_TASK_NAME", " \"^$MU_TASK_REF\""),
    };
    let after = match result {
        Leaves::Nothing => String::new(),
        Leaves::Changed | Leaves::Forged => format!(
            r#"echo worked > a.txt
git add -A && git -c user.name=t -c user.email=t@t commit -q -m work
git update-ref "{ref_name}" HEAD
git bundle create -q "$MU_TASK_RESULT_BUNDLE" "{ref_name}"{with_base}"#
        ),
    };
    let script = format!(
        r#"#!/bin/sh
PATH='{path}'; export PATH
unset MU_GATEWAY_URL ANTHROPIC_API_KEY
env > '{log}'/env
W='{bare}'
d="$W/work/$MU_TASK_NAME"
git init -q "$d" && git -C "$d" fetch -q "$MU_TASK_BUNDLE" "$MU_TASK_REF:$MU_TASK_REF" \
  && git -C "$d" checkout -q --detach "$MU_TASK_REF" || exit 1
cd "$d"
MU_HOME="$W" HOME="$W" MU_TEST_HOME="$W" '{mu}' --headless | tee -a '{log}'/from-worker
{after}
"#,
        path = std::env::var("PATH").unwrap(),
        log = log.display(),
        bare = bare.display(),
        mu = env!("CARGO_BIN_EXE_mu"),
    );
    let launcher = home.join("launcher.sh");
    std::fs::write(&launcher, script).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!("[task]\nlauncher = [{:?}]\nlaunch = [\"task\"]\n", launcher.display().to_string()),
    )
    .unwrap();
    Launcher { bare, log }
}

/// A launched child has no credentials and no gateway: its model calls reach the server only
/// through the parent.
#[test]
fn a_launched_child_with_no_credentials_reaches_the_model_through_the_parent() {
    let (home, cwd, seen) = relay_fixture("relayed child result");
    let launcher = launcher_for(&home, Leaves::Nothing);
    let o = mu(&home, &cwd).args(["-p", "delegate it", "--model", MODEL]).output().unwrap();
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let requests = seen.lock().unwrap();
    assert!(!child_requests(&requests).is_empty(), "the server saw the child's request: {requests:?}");
    assert!(
        requests.iter().any(|r| r.contains("\"tool_result\"") && r.contains("relayed child result")),
        "the child's result reaches the parent's next request"
    );
    assert!(stdout(&o).contains("parent finished"), "stdout: {}", stdout(&o));
    assert!(!launcher.bare.join("auth.json").exists(), "the child never held a key store");
    let env = launcher.log("env");
    assert!(
        env.contains("MU_TASK_CONTRACT=2") && env.contains("MU_TASK_BUNDLE=") && env.contains("MU_TASK_RESULT_BUNDLE=")
    );
    assert!(!env.contains("sk-test-key"), "the launcher is given no credential: {env}");
    assert!(!launcher.log("from-worker").is_empty(), "the stream went through the launcher");
}

/// The result bundle the launcher leaves is imported and applied to the parent's tree.
#[test]
fn a_launched_childs_result_bundle_is_imported_and_applied() {
    let (home, cwd, seen) = relay_fixture("relayed child result");
    let _launcher = launcher_for(&home, Leaves::Changed);
    let o = mu(&home, &cwd).args(["-p", "delegate it", "--model", MODEL]).output().unwrap();
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "worked\n");
    let requests = seen.lock().unwrap();
    assert!(
        requests.iter().any(|r| r.contains("\"tool_result\"") && r.contains("[changes: applied: 1 files")),
        "the parent is told: {requests:?}"
    );
    assert_eq!(mu_testutil::sh(&cwd, &["for-each-ref", "refs/mu/task/"]), "", "the task ref is gone");
}

/// A bundle that holds a ref the launch does not own is refused, and nothing reaches the tree.
#[test]
fn a_forged_result_bundle_is_refused() {
    let (home, cwd, seen) = relay_fixture("relayed child result");
    let _launcher = launcher_for(&home, Leaves::Forged);
    let o = mu(&home, &cwd).args(["-p", "delegate it", "--model", MODEL]).output().unwrap();
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert_eq!(std::fs::read_to_string(cwd.join("a.txt")).unwrap(), "one\n");
    let requests = seen.lock().unwrap();
    assert!(
        requests.iter().any(|r| r.contains("\"tool_result\"") && r.contains("does not hold exactly")),
        "{requests:?}"
    );
    assert_eq!(mu_testutil::sh(&cwd, &["for-each-ref", "refs/mu/result/", "refs/heads/forged"]), "");
}

/// The launched child's session is mirrored into the parent's session directory.
#[test]
fn a_launched_childs_session_is_mirrored_in_the_parent() {
    fn find(dir: &Path) -> bool {
        std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
            let p = e.path();
            if p.is_dir() {
                find(&p)
            } else {
                p.extension().is_some_and(|x| x == "jsonl")
                    && std::fs::read_to_string(&p).is_ok_and(|t| t.contains("\"mirrored\":true"))
            }
        })
    }
    let (home, cwd, _) = relay_fixture("relayed child result");
    let _launcher = launcher_for(&home, Leaves::Nothing);
    let o = mu(&home, &cwd).args(["-p", "delegate it", "--model", MODEL]).output().unwrap();
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    assert!(find(&home), "a mirrored session file is in the parent's home");
}

/// The worker itself refuses a payload of another version, whoever sent it.
#[test]
fn headless_refuses_a_task_of_another_version() {
    let home = home_with_key(&serve(vec![]));
    let cwd = mu_testutil::tmp();
    let payload = json!({"agent": "task", "task": "x", "muVersion": "0.0.0"}).to_string();
    let mut c = mu(&home, &cwd);
    let o = run_with_stdin(c.args(["--headless", "--model", MODEL]), &payload);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("use the same version on both"), "stderr: {}", stderr(&o));
    assert!(stdout(&o).is_empty(), "no result line: {}", stdout(&o));
}

/// What a relayed worker spends counts against `--max-cost`: the child's one reply costs more than
/// the cap, and the run fails; the same run without relaying (the child pays itself) does not.
#[test]
fn max_cost_counts_what_a_relayed_worker_spent() {
    let run = |relayed: bool| {
        let (home, cwd, _) = relay_fixture_from(relay_model_spending("expensive child", 1_000_000));
        let mut c = mu(&home, &cwd);
        if relayed {
            c.env("MU_TASK_RELAY", "1");
        }
        c.args(["-p", "delegate it", "--model", MODEL, "--max-cost", "1.0"]).output().unwrap()
    };
    let o = run(true);
    assert!(!o.status.success(), "stdout: {} stderr: {}", stdout(&o), stderr(&o));
    assert!(stderr(&o).contains("cost cap $1.00 exceeded"), "stderr: {}", stderr(&o));
    let o = run(false);
    assert!(o.status.success(), "stdout: {} stderr: {}", stdout(&o), stderr(&o));
}

/// `MU_TASK_RELAY=1` relays for a local child too.
#[test]
fn a_local_child_is_relayed_when_the_environment_asks() {
    let (home, cwd, seen) = relay_fixture("local relayed result");
    let o = mu(&home, &cwd).env("MU_TASK_RELAY", "1").args(["-p", "delegate it", "--model", MODEL]).output().unwrap();
    assert!(o.status.success(), "stderr: {}", stderr(&o));
    let requests = seen.lock().unwrap();
    assert!(!child_requests(&requests).is_empty(), "the server saw the child's request: {requests:?}");
    assert!(
        requests.iter().any(|r| r.contains("\"tool_result\"") && r.contains("local relayed result")),
        "the child's result reaches the parent's next request"
    );
    assert!(stdout(&o).contains("parent finished"), "stdout: {}", stdout(&o));
}
