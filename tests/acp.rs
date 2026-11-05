//! `mu --acp` end to end: the real binary speaking ACP over pipes, with a private `MU_HOME` and cwd
//! and an Anthropic API served by a local fake. The test plays the client.
use mu_testutil::{Reply, block_on, fake_server};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const MODEL: &str = "anthropic/claude-sonnet-5-5";

fn frames(blocks: &[Value], stop: &str) -> String {
    let mut out =
        String::from("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\n");
    for (i, b) in blocks.iter().enumerate() {
        out.push_str(&format!(
            "event: content_block_start\ndata: {}\n\n",
            json!({"type": "content_block_start", "index": i, "content_block": b["start"]})
        ));
        out.push_str(&format!(
            "event: content_block_delta\ndata: {}\n\n",
            json!({"type": "content_block_delta", "index": i, "delta": b["delta"]})
        ));
        out.push_str(&format!(
            "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{i}}}\n\n"
        ));
    }
    out.push_str(&format!(
        "event: message_delta\ndata: {}\n\n",
        json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"input_tokens": 10, "output_tokens": 2}})
    ));
    out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    out
}

fn sse(body: String) -> Reply {
    Reply::new(200, &body).header("content-type", "text/event-stream")
}

fn text(t: &str) -> Reply {
    sse(frames(
        &[json!({"start": {"type": "text", "text": ""}, "delta": {"type": "text_delta", "text": t}})],
        "end_turn",
    ))
}

fn tool_use(id: &str, name: &str, input: Value) -> Reply {
    sse(frames(
        &[json!({
            "start": {"type": "tool_use", "id": id, "name": name, "input": {}},
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()},
        })],
        "tool_use",
    ))
}

type Seen = Arc<Mutex<Vec<String>>>;

/// Serves `replies` in order from a background thread; the base URL and the requests it saw.
fn serve(replies: Vec<Reply>) -> (String, Seen) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        block_on(async move {
            let (base, seen) = fake_server(replies).await;
            tx.send((base, seen)).unwrap();
            std::future::pending::<()>().await;
        })
    });
    rx.recv().unwrap()
}

fn home_with_key(base: &str) -> PathBuf {
    let home = mu_testutil::tmp();
    let store = json!({"version": 1, "providers": {"anthropic": {"key": "sk-test-key", "baseUrl": base}}});
    std::fs::write(home.join("auth.json"), store.to_string()).unwrap();
    std::fs::create_dir_all(home.join("empty-bin")).unwrap();
    home
}

struct Acp {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    /// Every message the agent sent, in order.
    log: Vec<Value>,
    next: u64,
}

impl Drop for Acp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the client answers to a request of the agent: `(method, params)` to the result.
type Answer<'a> = &'a mut dyn FnMut(&str, &Value) -> Value;

fn nothing(_: &str, _: &Value) -> Value {
    json!({})
}

impl Acp {
    fn start(home: &Path, cwd: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mu"))
            .args(["--acp", "--model", MODEL])
            .current_dir(cwd)
            .env("MU_TEST_HOME", home)
            .env("MU_HOME", home)
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env("MU_OMP", home.join("no-omp"))
            .env_remove("OMP_PROFILE")
            .env_remove("MU_GATEWAY_URL")
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        Self { child, stdin, rx, log: Vec::new(), next: 1 }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Takes one message from the agent; a request of the agent's is answered by `answer`.
    fn pump(&mut self, wait: Duration, answer: Answer) -> Option<Value> {
        let m = self.rx.recv_timeout(wait).ok()?;
        self.log.push(m.clone());
        if let (Some(method), Some(id)) = (m["method"].as_str(), m.get("id")) {
            let result = answer(method, &m["params"]);
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
        }
        Some(m)
    }

    /// Sends a request and returns its response, answering the agent's requests meanwhile.
    fn call_with(&mut self, method: &str, params: Value, answer: Answer) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if let Some(m) = self.pump(Duration::from_secs(1), answer)
                && m["id"] == json!(id)
                && m["method"].is_null()
            {
                return m;
            }
        }
        panic!("no response to {method}; log: {:#?}", self.log);
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.call_with(method, params, &mut nothing)
    }

    /// Keeps reading until some message matches, or panics after `secs`.
    fn wait_for(&mut self, what: &str, secs: u64, pred: impl Fn(&Value) -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while !self.log.iter().any(&pred) {
            if Instant::now() > end {
                panic!("never saw {what}; log: {:#?}", self.log);
            }
            self.pump(Duration::from_millis(200), &mut nothing);
        }
    }

    fn updates(&self) -> Vec<&Value> {
        self.log.iter().filter(|m| m["method"] == "session/update").map(|m| &m["params"]["update"]).collect()
    }

    fn init(&mut self, caps: Value) {
        let r = self.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": caps}));
        assert_eq!(r["result"]["protocolVersion"], 1, "{r}");
    }

    fn new_session(&mut self, cwd: &Path) -> (String, Value) {
        let r = self.call("session/new", json!({"cwd": cwd, "mcpServers": []}));
        let sid = r["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("no session: {r}")).to_string();
        (sid, r["result"].clone())
    }

    fn prompt_with(&mut self, sid: &str, text: &str, answer: Answer) -> Value {
        self.call_with("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}), answer)
    }

    fn prompt(&mut self, sid: &str, text: &str) -> Value {
        self.prompt_with(sid, text, &mut nothing)
    }
}

fn setup(replies: Vec<Reply>) -> (Acp, PathBuf, Seen) {
    let (base, seen) = serve(replies);
    let home = home_with_key(&base);
    let cwd = mu_testutil::tmp();
    (Acp::start(&home, &cwd), cwd, seen)
}

fn kind_of(u: &Value) -> &str {
    u["sessionUpdate"].as_str().unwrap_or("")
}

fn call_text(u: &Value) -> String {
    u["content"]
        .as_array()
        .map(|c| c.iter().filter_map(|x| x["content"]["text"].as_str()).collect::<Vec<_>>().join(""))
        .unwrap_or_default()
}

#[test]
fn a_session_offers_modes_and_commands_and_a_prompt_streams_its_reply() {
    let (mut acp, cwd, _) = setup(vec![text("hello from the fake")]);
    acp.init(json!({}));
    let (sid, res) = acp.new_session(&cwd);
    let modes: Vec<&str> =
        res["modes"]["availableModes"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!((modes, res["modes"]["currentModeId"].as_str()), (vec!["default", "plan"], Some("default")));
    let mode_option = res["configOptions"].as_array().unwrap().iter().find(|o| o["category"] == "mode").unwrap();
    assert_eq!(mode_option["currentValue"], "default");
    let r = acp.prompt(&sid, "hi");
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    let ups = acp.updates();
    let cmds = ups.iter().find(|u| kind_of(u) == "available_commands_update").expect("commands are advertised");
    let names: Vec<&str> =
        cmds["availableCommands"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    for want in ["compact", "review", "plan", "rename"] {
        assert!(names.contains(&want), "{names:?}");
    }
    assert!(ups.iter().any(|u| kind_of(u) == "agent_message_chunk" && u["content"]["text"] == "hello from the fake"));
}

#[test]
fn a_failed_turn_is_an_error_response_not_end_turn() {
    let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let (mut acp, cwd, _) = setup(vec![Reply::new(401, body)]);
    acp.init(json!({}));
    let (sid, _) = acp.new_session(&cwd);
    let r = acp.prompt(&sid, "hi");
    assert_eq!(r["error"]["code"], -32603, "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("invalid x-api-key"), "{r}");
    assert!(
        acp.updates()
            .iter()
            .any(|u| kind_of(u) == "agent_message_chunk" && u["content"]["text"].as_str().unwrap().contains("Error"))
    );
}

#[test]
fn tool_calls_carry_locations_diffs_and_streamed_output() {
    let (mut acp, cwd, _) = setup(vec![
        tool_use("t1", "write", json!({"path": "hello.txt", "content": "hi\n"})),
        tool_use("t2", "bash", json!({"command": "echo one; sleep 1; echo two"})),
        text("done"),
    ]);
    acp.init(json!({}));
    let (sid, _) = acp.new_session(&cwd);
    let r = acp.prompt(&sid, "go");
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    assert_eq!(std::fs::read_to_string(cwd.join("hello.txt")).unwrap(), "hi\n");
    let ups = acp.updates();
    let call = ups.iter().find(|u| kind_of(u) == "tool_call" && u["toolCallId"] == "t1").unwrap();
    assert_eq!((call["kind"].as_str(), call["name"].as_str()), (Some("edit"), Some("write")));
    assert_eq!(call["locations"][0]["path"], json!(cwd.join("hello.txt")));
    let done = ups
        .iter()
        .find(|u| kind_of(u) == "tool_call_update" && u["toolCallId"] == "t1" && u["status"] == "completed")
        .unwrap();
    let diff = &done["content"][0];
    assert_eq!(
        (diff["type"].as_str(), diff["newText"].as_str(), diff["oldText"].is_null()),
        (Some("diff"), Some("hi\n"), true),
        "{done}"
    );
    assert_eq!(diff["path"], json!(cwd.join("hello.txt")));
    // The command's output shows up while it runs, before it ends.
    let running: Vec<String> = ups
        .iter()
        .filter(|u| kind_of(u) == "tool_call_update" && u["toolCallId"] == "t2" && u["status"] == "in_progress")
        .map(|u| call_text(u))
        .collect();
    assert!(running.iter().any(|t| t.contains("one") && !t.contains("two")), "{running:?}");
    let end = ups
        .iter()
        .find(|u| kind_of(u) == "tool_call_update" && u["toolCallId"] == "t2" && u["status"] == "completed")
        .unwrap();
    assert!(call_text(end).contains("one\ntwo"), "{end}");
    assert_eq!(end["rawOutput"]["exitCode"], 0, "{end}");
}

#[test]
fn slash_commands_run_without_the_model_and_modes_switch() {
    let (mut acp, cwd, seen) = setup(vec![text("unused")]);
    acp.init(json!({}));
    let (sid, _) = acp.new_session(&cwd);
    let r = acp.prompt(&sid, "/rename Smoke test");
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    acp.wait_for("the new title", 10, |m| {
        m["params"]["update"]["sessionUpdate"] == "session_info_update"
            && m["params"]["update"]["title"] == "Smoke test"
    });
    let r = acp.prompt(&sid, "/skill:nope");
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    assert!(acp.updates().iter().any(|u| kind_of(u) == "agent_message_chunk"
        && u["content"]["text"].as_str().unwrap().contains("unknown skill nope")));
    // Plan mode is a mode, reachable both ways.
    let r = acp.call("session/set_mode", json!({"sessionId": sid, "modeId": "plan"}));
    assert_eq!(r["result"], json!({}), "{r}");
    acp.wait_for("the mode update", 10, |m| {
        m["params"]["update"]["sessionUpdate"] == "current_mode_update"
            && m["params"]["update"]["currentModeId"] == "plan"
    });
    let r = acp.call("session/set_config_option", json!({"sessionId": sid, "configId": "mode", "value": "default"}));
    let mode = r["result"]["configOptions"].as_array().unwrap().iter().find(|o| o["id"] == "mode").unwrap().clone();
    assert_eq!(mode["currentValue"], "default", "{r}");
    let r = acp.call("session/set_mode", json!({"sessionId": sid, "modeId": "bogus"}));
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert!(seen.lock().unwrap().is_empty(), "none of that reached the model");
}

#[test]
fn a_plan_is_approved_through_a_permission_request() {
    let (mut acp, cwd, _) = setup(vec![
        tool_use("w1", "write", json!({"path": "local://demo-plan.md", "content": "# Demo plan\n"})),
        tool_use("p1", "propose_plan", json!({"slug": "demo", "title": "Demo"})),
        text("parked"),
    ]);
    acp.init(json!({}));
    let (sid, _) = acp.new_session(&cwd);
    let r = acp.call("session/set_mode", json!({"sessionId": sid, "modeId": "plan"}));
    assert_eq!(r["result"], json!({}), "{r}");
    let mut asked: Vec<Value> = Vec::new();
    let r = acp.prompt_with(&sid, "plan it", &mut |method, params| {
        assert_eq!(method, "session/request_permission");
        asked.push(params.clone());
        json!({"outcome": {"outcome": "selected", "optionId": "park"}})
    });
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    assert_eq!(asked.len(), 1);
    let ids: Vec<&str> =
        asked[0]["options"].as_array().unwrap().iter().map(|o| o["optionId"].as_str().unwrap()).collect();
    assert_eq!(ids, ["keep", "execute", "compact", "park", "refine"]);
    assert_eq!(
        (asked[0]["toolCall"]["kind"].as_str(), asked[0]["toolCall"]["toolCallId"].as_str()),
        (Some("switch_mode"), Some("p1"))
    );
    assert!(asked[0]["toolCall"]["content"][0]["content"]["text"].as_str().unwrap().contains("# Demo plan"));
    // Parking ends plan mode.
    acp.wait_for("the default mode again", 10, |m| {
        m["params"]["update"]["sessionUpdate"] == "current_mode_update"
            && m["params"]["update"]["currentModeId"] == "default"
    });
}

#[test]
fn ask_is_offered_only_to_a_client_that_takes_forms() {
    let (mut acp, cwd, seen) = setup(vec![text("fine")]);
    acp.init(json!({}));
    let (sid, _) = acp.new_session(&cwd);
    acp.prompt(&sid, "hi");
    let first = seen.lock().unwrap()[0].clone();
    assert!(!first.contains("\"name\":\"ask\""), "no form support, no ask tool");
    assert!(first.contains("\"name\":\"read\""), "other tools stay");

    let q = json!({"questions": [{"id": "pick", "question": "Which?", "options": [{"label": "A"}, {"label": "B"}]}]});
    let (mut acp, cwd, seen) = setup(vec![tool_use("a1", "ask", q), text("thanks")]);
    acp.init(json!({"elicitation": {"form": {}}}));
    let (sid, _) = acp.new_session(&cwd);
    let mut forms: Vec<Value> = Vec::new();
    let r = acp.prompt_with(&sid, "ask me", &mut |method, params| {
        assert_eq!(method, "elicitation/create");
        forms.push(params.clone());
        json!({"action": "accept", "content": {"q0": "B"}})
    });
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0]["mode"], "form");
    assert_eq!(forms[0]["requestedSchema"]["properties"]["q0"]["oneOf"][1]["const"], "B");
    let second = seen.lock().unwrap()[1].clone();
    assert!(second.contains("pick: B"), "the answer reaches the model: {second}");
    assert!(seen.lock().unwrap()[0].contains("\"name\":\"ask\""));
}

#[test]
fn a_background_job_is_a_tool_call_until_it_ends() {
    let (mut acp, cwd, _) = setup(vec![
        tool_use("b1", "bash", json!({"command": "echo job-out; sleep 1", "async": true})),
        text("started"),
        text("noted"),
    ]);
    acp.init(json!({}));
    let (sid, _) = acp.new_session(&cwd);
    let r = acp.prompt(&sid, "run it in the background");
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    acp.wait_for("the job finishing", 20, |m| {
        let u = &m["params"]["update"];
        u["sessionUpdate"] == "tool_call_update"
            && u["toolCallId"].as_str().is_some_and(|i| i.starts_with("job:"))
            && u["status"] == "completed"
    });
    let ups = acp.updates();
    let start = ups
        .iter()
        .find(|u| kind_of(u) == "tool_call" && u["toolCallId"].as_str().is_some_and(|i| i.starts_with("job:")))
        .unwrap();
    assert_eq!((start["kind"].as_str(), start["status"].as_str()), (Some("execute"), Some("in_progress")));
}

#[test]
fn a_notice_reaches_a_client_without_notice_support_as_part_of_the_reply() {
    let retry = Reply::new(429, "slow down").header("retry-after-ms", "20");
    let (mut acp, cwd, _) = setup(vec![retry, text("after the wait")]);
    acp.init(json!({}));
    let (sid, _) = acp.new_session(&cwd);
    let r = acp.prompt(&sid, "hi");
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    assert!(
        acp.updates()
            .iter()
            .any(|u| kind_of(u) == "agent_message_chunk"
                && u["content"]["text"].as_str().unwrap().contains("retrying in")),
        "{:#?}",
        acp.updates()
    );

    let retry = Reply::new(429, "slow down").header("retry-after-ms", "20");
    let (mut acp, cwd, _) = setup(vec![retry, text("after the wait")]);
    acp.init(json!({"session": {"notices": {}}}));
    let (sid, _) = acp.new_session(&cwd);
    acp.prompt(&sid, "hi");
    let ups = acp.updates();
    let n = ups.iter().find(|u| kind_of(u) == "notice").expect("a notice update");
    assert_eq!(n["severity"], "warning");
    assert!(!ups.iter().any(|u| kind_of(u) == "agent_message_chunk" && u["content"]["text"].as_str().unwrap().contains("retrying")));
}

#[test]
fn a_deleted_session_leaves_the_list() {
    let (mut acp, cwd, _) = setup(vec![text("unused")]);
    acp.init(json!({}));
    let (sid, _) = acp.new_session(&cwd);
    let listed = |acp: &mut Acp| -> Vec<String> {
        let r = acp.call("session/list", json!({"cwd": cwd}));
        r["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["sessionId"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(listed(&mut acp).contains(&sid));
    let r = acp.call("session/delete", json!({"sessionId": sid}));
    assert_eq!(r["result"], json!({}), "{r}");
    assert!(!listed(&mut acp).contains(&sid));
    let r = acp.call("session/delete", json!({"sessionId": sid}));
    assert_eq!(r["result"], json!({}), "a session that is gone is deleted already: {r}");
}
