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
fn home_with_key(base: &str) -> PathBuf {
    let home = mu_testutil::tmp();
    let store = json!({"version": 1, "providers": {"anthropic": {"key": "sk-test-key", "baseUrl": base}}});
    std::fs::write(home.join("auth.json"), store.to_string()).unwrap();
    std::fs::create_dir_all(home.join("empty-bin")).unwrap();
    home
}

/// The binary with a private home and cwd, and an empty `PATH` so no omp is found.
fn mu(home: &Path, cwd: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mu"));
    c.current_dir(cwd)
        .env("MU_TEST_HOME", home)
        .env("MU_HOME", home)
        .env("HOME", home)
        .env("PATH", home.join("empty-bin"))
        .env("MU_OMP", home.join("no-omp"))
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
    let sse_body = format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m1\"}}}}\n\n\
         event: content_block_start\ndata: {}\n\n\
         event: content_block_delta\ndata: {}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"input_tokens\":10,\"output_tokens\":2}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
    );
    sse(&sse_body)
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
