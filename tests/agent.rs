//! `mu agent` and `mu attach`: the real binary, a private `MU_HOME` and cwd, a fake Anthropic API, and a
//! scripted client speaking the wire lines over the agent's Unix socket.
use mu_base::events::Command as AgentCommand;
use mu_base::wire::{ClientLine, WIRE, WireCommand, to_line};
use mu_testutil::{Reply, block_on, fake_server};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const MODEL: &str = "anthropic/claude-sonnet-5-5";

const ANSWER: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\n\
    event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
    event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello from the fake\"}}\n\n\
    event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
    event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}\n\n\
    event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

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

struct Agent {
    child: Child,
    socket: PathBuf,
    session: String,
}

impl Agent {
    fn start(home: &Path, cwd: &Path, extra: &[&str]) -> Agent {
        let mut child = mu(home, cwd)
            .args(["agent", "--model", MODEL, "--permissions", "yolo"])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut first = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut first).unwrap();
        let v: Value = serde_json::from_str(&first).unwrap_or_else(|e| panic!("first line {first:?}: {e}"));
        Agent {
            socket: PathBuf::from(v["socket"].as_str().unwrap()),
            session: v["session"].as_str().unwrap().to_string(),
            child,
        }
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        for _ in 0..100 {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the agent did not end");
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        let stream = UnixStream::connect(socket).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Client { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream }
    }

    fn send(&mut self, line: &ClientLine) {
        writeln!(self.writer, "{}", to_line(line)).unwrap();
    }

    fn hello(&mut self) -> Value {
        self.send(&ClientLine::Hello { wire: WIRE, version: "test".into() });
        self.next().expect("the agent's hello")
    }

    /// The next line, `None` at the end of the stream.
    fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"))),
            Err(e) => panic!("reading the agent: {e}"),
        }
    }

    /// Reads until `done` accepts a line; returns all lines read, that one last.
    fn until(&mut self, done: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(v) = self.next() {
            let stop = done(&v);
            out.push(v);
            if stop {
                return out;
            }
        }
        panic!("the stream ended before the line was seen: {out:?}");
    }

    fn command(&mut self, c: AgentCommand) {
        self.send(&ClientLine::Command(WireCommand::Command(c)));
    }
}

fn event_is(v: &Value, name: &str) -> bool {
    let e = &v["event"]["Event"];
    e == name || e.get(name).is_some()
}

fn caught_up(v: &Value) -> bool {
    v["event"] == "CaughtUp"
}

fn fixture() -> (PathBuf, PathBuf) {
    let home = home_with_key(&serve(vec![sse(ANSWER), sse(ANSWER), sse(ANSWER)]));
    (home, mu_testutil::tmp())
}

#[test]
fn an_agent_prints_its_socket_and_serves_a_prompt_then_exits_on_exit() {
    let (home, cwd) = fixture();
    let mut agent = Agent::start(&home, &cwd, &[]);
    let mode = std::fs::metadata(&agent.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the socket is private");
    assert_eq!(agent.socket.parent().unwrap(), home.join("run"), "beside the owner record");
    assert!(agent.socket.file_name().unwrap().to_string_lossy().starts_with(&agent.session));

    let mut c = Client::connect(&agent.socket);
    let hello = c.hello();
    assert_eq!(hello["hello"]["wire"], WIRE);
    assert!(hello["hello"]["host"]["cwd"].is_string(), "the host's facts: {hello}");
    c.until(caught_up);

    c.command(AgentCommand::Prompt("hi".into()));
    let seen = c.until(|v| event_is(v, "TurnEnd"));
    assert!(seen.iter().any(|v| v["event"]["Event"]["Text"] == "hello from the fake"), "{seen:?}");

    c.command(AgentCommand::Exit);
    let rest = c.until(|v| event_is(v, "Exit"));
    assert!(!rest.is_empty());
    assert!(c.next().is_none(), "the connection closes with the agent");
    assert!(agent.wait_exit().success());
    assert!(!agent.socket.exists(), "the socket goes with the agent");
}

#[test]
fn a_different_wire_number_is_refused() {
    let (home, cwd) = fixture();
    let agent = Agent::start(&home, &cwd, &[]);
    let mut c = Client::connect(&agent.socket);
    c.send(&ClientLine::Hello { wire: WIRE + 1, version: "test".into() });
    let reply = c.next().unwrap();
    assert!(reply["error"].as_str().unwrap().contains("wire"), "{reply}");
    assert!(c.next().is_none(), "and the connection closes");
}

#[test]
fn a_detached_client_leaves_the_agent_running_and_the_next_one_sees_the_conversation() {
    let (home, cwd) = fixture();
    let mut agent = Agent::start(&home, &cwd, &[]);
    let mut c = Client::connect(&agent.socket);
    c.hello();
    c.until(caught_up);
    c.command(AgentCommand::Prompt("remember me".into()));
    c.until(|v| event_is(v, "TurnEnd"));
    c.send(&ClientLine::Detach);
    assert!(c.next().is_none() || c.next().is_none(), "the connection closes");
    assert!(agent.child.try_wait().unwrap().is_none(), "the agent runs on");

    let mut again = Client::connect(&agent.socket);
    again.hello();
    let lines = again.until(caught_up);
    let prompts: Vec<&Value> = lines.iter().filter(|v| v["event"]["Event"].get("UserPrompt").is_some()).collect();
    assert_eq!(prompts.len(), 1, "history is replayed: {lines:?}");
    assert_eq!(prompts[0]["event"]["Event"]["UserPrompt"], "remember me");
    assert!(lines.iter().any(|v| v["event"]["Event"].get("Text").is_some()), "the reply too");
}

#[test]
fn a_second_client_takes_the_session_and_the_first_is_told() {
    let (home, cwd) = fixture();
    let agent = Agent::start(&home, &cwd, &[]);
    let mut first = Client::connect(&agent.socket);
    first.hello();
    first.until(caught_up);
    let mut second = Client::connect(&agent.socket);
    second.hello();
    second.until(caught_up);
    let told = first.until(|v| v["event"].get("Detached").is_some());
    assert!(told.last().unwrap()["event"]["Detached"]["reason"].is_string());
    assert!(first.next().is_none(), "and its connection closes");
    second.command(AgentCommand::Prompt("still here".into()));
    second.until(|v| event_is(v, "TurnEnd"));
}

#[test]
fn a_host_request_is_answered_on_the_same_stream() {
    let (home, cwd) = fixture();
    let agent = Agent::start(&home, &cwd, &[]);
    let mut c = Client::connect(&agent.socket);
    c.hello();
    c.until(caught_up);
    c.send(&ClientLine::Host { id: 7, call: "skills".into(), args: json!(null) });
    let reply = c.until(|v| v.get("hostReply").is_some()).pop().unwrap();
    assert_eq!(reply["hostReply"]["id"], 7);
    assert!(reply["hostReply"]["result"]["Ok"].is_array(), "{reply}");
    c.send(&ClientLine::Host { id: 8, call: "no-such-call".into(), args: json!(null) });
    let reply = c.until(|v| v.get("hostReply").is_some()).pop().unwrap();
    assert!(reply["hostReply"]["result"]["Err"].as_str().unwrap().contains("no-such-call"), "{reply}");
}

#[test]
fn sigterm_ends_the_agent_and_removes_its_socket() {
    let (home, cwd) = fixture();
    let mut agent = Agent::start(&home, &cwd, &[]);
    assert!(agent.socket.exists());
    unsafe { libc::kill(agent.child.id() as i32, libc::SIGTERM) };
    agent.wait_exit();
    assert!(!agent.socket.exists());
}

#[test]
fn attach_copies_lines_between_stdio_and_the_socket() {
    let (home, cwd) = fixture();
    let agent = Agent::start(&home, &cwd, &[]);
    let mut attach = mu(&home, &cwd)
        .args(["attach", &agent.session[..8]])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = attach.stdin.take().unwrap();
    writeln!(stdin, "{}", to_line(&ClientLine::Hello { wire: WIRE, version: "test".into() })).unwrap();
    writeln!(stdin, "{}", to_line(&ClientLine::Detach)).unwrap();
    drop(stdin);
    let out = attach.wait_with_output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.lines().next().unwrap().contains("\"hello\""), "{text}");
    assert!(text.contains("CaughtUp"), "{text}");
}

#[test]
fn attach_with_nothing_running_says_so() {
    let (home, cwd) = fixture();
    let o = mu(&home, &cwd).args(["attach"]).output().unwrap();
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("no agent is running"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
