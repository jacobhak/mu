//! `mu agent` and `mu attach`: the real binary, a private `MU_HOME` and cwd, a fake Anthropic API, and a
//! scripted client speaking the wire lines over the agent's Unix socket.
use mu_base::events::Command as AgentCommand;
use mu_base::wire::{ClientLine, WIRE, WireCommand, to_line};
use mu_testutil::{Reply, block_on, fake_server};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
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
fn home_with_key(base: &str) -> mu_testutil::TempDir {
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
        Agent::start_env(home, cwd, extra, &[])
    }

    fn start_env(home: &Path, cwd: &Path, extra: &[&str], env: &[(&str, &Path)]) -> Agent {
        let mut command = mu(home, cwd);
        let mode: &[&str] = if extra.contains(&"--permissions") { &[] } else { &["--permissions", "yolo"] };
        command
            .envs(env.iter().copied())
            .args(["agent", "--model", MODEL])
            .args(mode)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // SAFETY: umask is async-signal-safe. A umask of 0 shows the agent sets its own modes.
        unsafe {
            command.pre_exec(|| {
                libc::umask(0);
                Ok(())
            })
        };
        let mut child = command.spawn().unwrap();
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

/// A client of the wire over any pipe: plain JSON lines, the hello first.
struct Client {
    reader: Box<dyn BufRead>,
    writer: Box<dyn Write>,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        let stream = UnixStream::connect(socket).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Client::over(Box::new(BufReader::new(stream.try_clone().unwrap())), Box::new(stream))
    }

    fn over(reader: Box<dyn BufRead>, writer: Box<dyn Write>) -> Client {
        Client { reader, writer }
    }

    /// An agent that refuses the version closes at once: a write after that may fail.
    fn write_text(&mut self, text: &str) {
        let _ = writeln!(self.writer, "{text}");
        let _ = self.writer.flush();
    }

    fn read_raw(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"))),
            Err(e) => panic!("reading the agent: {e}"),
        }
    }

    fn send(&mut self, line: &ClientLine) {
        self.write_text(&to_line(line));
    }

    /// A hello that takes the session from whoever has it.
    fn hello(&mut self) -> Value {
        self.send(&hello_line(WIRE, "test", true));
        self.next().expect("the agent's hello")
    }

    /// The next line, `None` at the end of the stream.
    fn next(&mut self) -> Option<Value> {
        self.read_raw()
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

fn hello_line(wire: u32, host: &str, take: bool) -> ClientLine {
    ClientLine::Hello {
        wire,
        version: "test".into(),
        reconnect: false,
        host: host.into(),
        client: format!("client-of-{host}"),
        take,
    }
}

fn event_is(v: &Value, name: &str) -> bool {
    let e = &v["event"]["Event"];
    e == name || e.get(name).is_some()
}

fn caught_up(v: &Value) -> bool {
    v["event"] == "CaughtUp"
}

fn fixture() -> (mu_testutil::TempDir, mu_testutil::TempDir) {
    let home = home_with_key(&serve(vec![sse(ANSWER), sse(ANSWER), sse(ANSWER)]));
    (home, mu_testutil::tmp())
}

#[test]
fn an_agent_prints_its_socket_and_serves_a_prompt_then_exits_on_exit() {
    let (home, cwd) = fixture();
    let mut agent = Agent::start(&home, &cwd, &[]);
    let mode = std::fs::metadata(&agent.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the socket is private");
    let run = std::fs::metadata(home.join("run")).unwrap().permissions().mode() & 0o777;
    assert_eq!(run, 0o700, "so is run/, under a umask of 0");
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
    c.send(&hello_line(WIRE + 1, "test", false));
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
        .args(["attach", "--stdio", &agent.session[..8]])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = attach.stdin.take().unwrap();
    let stdout = attach.stdout.take().unwrap();
    let mut c = Client::over(Box::new(BufReader::new(stdout)), Box::new(stdin));
    let hello = c.hello();
    assert!(hello.get("hello").is_some(), "{hello}");
    c.until(caught_up);
    c.send(&ClientLine::Detach);
    drop(c);
    let out = attach.wait_with_output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn attach_with_nothing_running_says_so() {
    let (home, cwd) = fixture();
    let o = mu(&home, &cwd).args(["attach", "--stdio"]).output().unwrap();
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("no agent is running"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn text_reply(text: &str) -> String {
    ANSWER.replace("hello from the fake", text)
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

/// A fake model that answers by request body (mu also sends title requests whose timing is not fixed):
/// the first real request calls `bash` on a terminal, the one after its result says it is done.
fn pty_model() -> String {
    tool_model(json!({"command": "echo from-the-pty", "pty": true}))
}

/// `pty_model` for any `bash` call.
fn tool_model(call: Value) -> String {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let call = call.clone();
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
                    text_reply("the terminal said it")
                } else {
                    tool_use_body("t1", "bash", &call)
                };
                let out = format!(
                    "HTTP/1.1 200 X\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(out.as_bytes());
            });
        }
    });
    base
}

#[test]
fn a_terminal_call_is_sized_and_fed_by_the_client() {
    let home = home_with_key(&pty_model());
    let cwd = mu_testutil::tmp();
    let agent = Agent::start(&home, &cwd, &[]);
    let mut c = Client::connect(&agent.socket);
    c.hello();
    c.until(caught_up);
    c.command(AgentCommand::Prompt("run it".into()));
    let open = c.until(|v| v["event"].get("PtyOpen").is_some()).pop().unwrap();
    let id = open["event"]["PtyOpen"]["id"].as_u64().unwrap();
    assert_eq!(open["event"]["PtyOpen"]["tool"], "t1");
    c.send(&ClientLine::Command(WireCommand::PtySize { id, cols: 80, rows: 24 }));
    let seen = c.until(|v| event_is(v, "TurnEnd"));
    let out: String = seen.iter().filter_map(|v| v["event"]["Event"]["ToolOutput"]["chunk"].as_str()).collect();
    assert!(out.contains("from-the-pty"), "the terminal's output reaches the client: {seen:?}");
    assert!(seen.iter().any(|v| v["event"]["Event"].get("ToolEnd").is_some()));
}

/// The session file of `id` under a test home.
fn session_file(home: &Path, id: &str) -> PathBuf {
    let mut stack = vec![home.join("sessions")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&format!("_{id}.jsonl"))) {
                return p;
            }
        }
    }
    panic!("no session file for {id}");
}

#[test]
fn a_session_is_taken_over_through_the_agent_and_reached_by_a_tunnel() {
    let (home, cwd) = fixture();
    // A session with one exchange in it, whose agent has ended.
    let mut first = Agent::start(&home, &cwd, &[]);
    let mut c = Client::connect(&first.socket);
    c.hello();
    c.until(caught_up);
    c.command(AgentCommand::Prompt("an earlier question".into()));
    c.until(|v| event_is(v, "TurnEnd"));
    c.command(AgentCommand::Exit);
    assert!(first.wait_exit().success());
    let file = session_file(&home, &first.session);

    // Another agent's client takes it over: the host starts an agent on it and a tunnel reaches it.
    let other_cwd = mu_testutil::tmp();
    let other = Agent::start(&home, &other_cwd, &[]);
    let mut c = Client::connect(&other.socket);
    c.hello();
    c.until(caught_up);
    c.send(&ClientLine::Host { id: 1, call: "take".into(), args: json!({"path": file}) });
    let reply = c.until(|v| v["hostReply"]["id"] == 1).pop().unwrap();
    let session =
        reply["hostReply"]["result"]["Ok"]["session"].as_str().unwrap_or_else(|| panic!("{reply}")).to_string();
    assert_eq!(session, first.session, "the resumed agent has the same session");

    c.send(&ClientLine::TunnelOpen { id: 7, session });
    // The nested client speaks to the second agent through the first one's tunnel.
    let tunnel = |c: &mut Client, line: &str| c.send(&ClientLine::TunnelLine { id: 7, line: line.to_string() });
    let next_tunnelled = |c: &mut Client| -> Value {
        loop {
            let v = c.next().expect("the tunnel carries lines");
            if let Some(line) = v["tunnelLine"]["line"].as_str() {
                return serde_json::from_str(line).unwrap();
            }
        }
    };
    tunnel(&mut c, &to_line(&hello_line(WIRE, "nested", false)));
    let tunnelled: Vec<Value> = {
        let mut got = Vec::new();
        loop {
            let inner = next_tunnelled(&mut c);
            let done = inner["event"] == "CaughtUp";
            got.push(inner);
            if done {
                break;
            }
        }
        got
    };
    assert!(tunnelled[0].get("hello").is_some(), "the second agent answers the nested hello: {tunnelled:?}");
    assert!(
        tunnelled.iter().any(|v| v["event"]["Event"]["UserPrompt"] == "an earlier question"),
        "and replays the conversation taken over: {tunnelled:?}"
    );

    // End the nested agent through the tunnel, then close it.
    tunnel(&mut c, &to_line(&ClientLine::Command(WireCommand::Command(AgentCommand::Exit))));
    let closed = c.until(|v| v.get("tunnelClose").is_some()).pop().unwrap();
    assert_eq!(closed["tunnelClose"]["id"], 7);
}

#[test]
fn a_tunnel_to_nothing_closes_with_a_reason() {
    let (home, cwd) = fixture();
    let agent = Agent::start(&home, &cwd, &[]);
    let mut c = Client::connect(&agent.socket);
    c.hello();
    c.until(caught_up);
    c.send(&ClientLine::TunnelOpen { id: 1, session: "no-such-agent".into() });
    let closed = c.until(|v| v.get("tunnelClose").is_some()).pop().unwrap();
    assert!(closed["tunnelClose"]["error"].as_str().unwrap().contains("cannot connect"), "{closed}");
    c.send(&ClientLine::TunnelOpen { id: 2, session: "../../etc".into() });
    let closed = c.until(|v| v.get("tunnelClose").is_some()).pop().unwrap();
    assert!(closed["tunnelClose"]["error"].as_str().unwrap().contains("not a session id"), "{closed}");
}

#[test]
fn a_terminal_nobody_attends_waits_in_the_agent_and_a_late_client_is_asked_again() {
    let home = home_with_key(&pty_model());
    let cwd = mu_testutil::tmp();
    let agent = Agent::start(&home, &cwd, &[]);
    let mut first = Client::connect(&agent.socket);
    first.hello();
    first.until(caught_up);
    first.command(AgentCommand::Prompt("run it".into()));
    first.until(|v| v["event"].get("PtyOpen").is_some());
    first.send(&ClientLine::Detach);

    let mut late = Client::connect(&agent.socket);
    late.hello();
    let lines = late.until(caught_up);
    let asked = lines.iter().find(|v| v["event"].get("PtyOpen").is_some()).expect("the request is sent again");
    let id = asked["event"]["PtyOpen"]["id"].as_u64().unwrap();
    late.send(&ClientLine::Command(WireCommand::PtySize { id, cols: 100, rows: 30 }));
    let seen = late.until(|v| event_is(v, "TurnEnd"));
    assert!(
        seen.iter()
            .any(|v| v["event"]["Event"]["ToolOutput"]["chunk"].as_str().is_some_and(|c| c.contains("from-the-pty")))
    );
}

/// Kills the process at the end of a test (a detached agent has no handle).
struct Pid(i32);

impl Drop for Pid {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

fn pid_of(home: &Path, id: &str) -> Pid {
    let text = std::fs::read_to_string(home.join("run").join(format!("{id}.json"))).unwrap();
    let rec: Value = serde_json::from_str(&text).unwrap();
    Pid(rec["pid"].as_u64().unwrap() as i32)
}

fn agent_list(home: &Path, cwd: &Path) -> Vec<Value> {
    let o = mu(home, cwd).args(["agent", "list", "--json"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..300 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn a_detached_agent_prints_its_id_and_survives_sighup() {
    let (home, cwd) = fixture();
    let o = mu(&home, &cwd)
        .args(["agent", "--detach", "--model", MODEL, "--permissions", "yolo"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
    let printed: Value = serde_json::from_slice(&o.stdout).unwrap();
    let id = printed["id"].as_str().unwrap().to_string();
    assert_eq!(o.stdout.iter().filter(|b| **b == b'\n').count(), 1, "one line: {printed}");
    let pid = pid_of(&home, &id);
    assert!(home.join("run").join(format!("{id}.sock")).exists(), "it listens before the command returns");
    assert!(home.join("logs").join(format!("agent-{id}.log")).is_file());

    unsafe { libc::kill(pid.0, libc::SIGHUP) };
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(unsafe { libc::kill(pid.0, 0) }, 0, "SIGHUP did not end it");
    let mut c = Client::connect(&home.join("run").join(format!("{id}.sock")));
    c.hello();
    c.until(caught_up);
    c.command(AgentCommand::Prompt("hi".into()));
    c.until(|v| event_is(v, "TurnEnd"));
    c.command(AgentCommand::Exit);
    c.until(|v| event_is(v, "Exit"));
}

#[test]
fn an_agent_that_cannot_make_its_session_file_says_why_and_exits() {
    let (home, cwd) = fixture();
    // The session directory cannot be made below a file.
    std::fs::write(home.join("sessions"), "").unwrap();
    let started = std::time::Instant::now();
    let o = mu(&home, &cwd)
        .args(["agent", "--detach", "--model", MODEL, "--permissions", "yolo"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success(), "stdout: {}", String::from_utf8_lossy(&o.stdout));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("cannot create the session file"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(30), "it did not wait for the start timeout");
}

#[test]
fn the_listing_says_who_is_attached_and_whether_a_request_waits() {
    let (home, cwd) = fixture();
    let agent = Agent::start(&home, &cwd, &[]);
    let rows = agent_list(&home, &cwd);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row["id"], agent.session.as_str());
    assert!(row["cwd"].is_string() && row["started"].as_i64().unwrap() > 1_000_000_000);
    assert!(row["title"].is_null() && row["client"].is_null() && row["waiting"] == false, "{row}");

    let mut c = Client::connect(&agent.socket);
    c.send(&hello_line(WIRE, "laptop", false));
    c.next().unwrap();
    c.until(caught_up);
    wait_until("the client in the listing", || agent_list(&home, &cwd)[0]["client"]["host"] == "laptop");
    let client = &agent_list(&home, &cwd)[0]["client"];
    assert!(client["since"].as_i64().unwrap() > 1_000_000_000, "{client}");

    c.send(&ClientLine::Detach);
    wait_until("the client to leave the listing", || agent_list(&home, &cwd)[0]["client"].is_null());
    let human = mu(&home, &cwd).args(["agent", "list"]).output().unwrap();
    assert!(String::from_utf8_lossy(&human.stdout).contains(&agent.session));
}

#[test]
fn a_second_client_is_told_who_has_the_session_and_takes_it_only_when_it_says_so() {
    let (home, cwd) = fixture();
    let agent = Agent::start(&home, &cwd, &[]);
    let mut alpha = Client::connect(&agent.socket);
    alpha.send(&hello_line(WIRE, "alpha", false));
    alpha.next().unwrap();
    alpha.until(caught_up);

    let mut beta = Client::connect(&agent.socket);
    beta.send(&hello_line(WIRE, "beta", false));
    let busy = beta.next().unwrap();
    assert_eq!(busy["busy"]["host"], "alpha", "{busy}");
    assert!(busy["busy"]["since"].as_i64().unwrap() > 1_000_000_000);
    assert!(beta.next().is_none(), "and the connection closes");
    // Alpha was not disturbed.
    alpha.command(AgentCommand::Prompt("still mine".into()));
    alpha.until(|v| event_is(v, "TurnEnd"));

    // A lost connection's successor is the same client: no question.
    let mut again = Client::connect(&agent.socket);
    again.send(&hello_line(WIRE, "alpha", false));
    assert!(again.next().unwrap().get("hello").is_some());
    again.until(caught_up);
    alpha.until(|v| v["event"].get("Detached").is_some());

    // The explicit take-over.
    let mut beta = Client::connect(&agent.socket);
    beta.send(&hello_line(WIRE, "beta", true));
    assert!(beta.next().unwrap().get("hello").is_some());
    beta.until(caught_up);
    again.until(|v| v["event"].get("Detached").is_some());
    wait_until("beta in the listing", || agent_list(&home, &cwd)[0]["client"]["host"] == "beta");
}

fn ask_mode_agent(home: &Path, cwd: &Path, config: &str) -> Agent {
    std::fs::write(home.join("config.toml"), config).unwrap();
    Agent::start(home, cwd, &["--permissions", "ask"])
}

fn allow() -> WireCommand {
    use mu_base::permissions::{ApprovalDecision, ApprovalScope};
    WireCommand::Approval {
        id: 1,
        decision: ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None, patterns: None },
    }
}

#[test]
fn an_approval_nobody_attends_is_flagged_announced_and_given_up_after_the_timeout() {
    let home = home_with_key(&tool_model(json!({"command": "echo x > made-by-the-test"})));
    let cwd = mu_testutil::tmp();
    let notified = home.join("notified");
    let config = format!("[agent]\nnotify_command = 'echo \"$1|$2\" > {}'\napproval_timeout = 3\n", notified.display());
    let agent = ask_mode_agent(&home, &cwd, &config);
    let mut c = Client::connect(&agent.socket);
    c.hello();
    c.until(caught_up);
    c.command(AgentCommand::Prompt("go".into()));
    c.until(|v| v["event"].get("Approve").is_some());
    assert!(agent_list(&home, &cwd)[0]["waiting"] == false, "a client is attached and sees it");
    c.send(&ClientLine::Detach);

    wait_until("the waiting flag", || agent_list(&home, &cwd)[0]["waiting"] == true);
    wait_until("the notify command", || notified.exists() && !std::fs::read_to_string(&notified).unwrap().is_empty());
    let said = std::fs::read_to_string(&notified).unwrap();
    assert!(said.starts_with(&format!("{}|approval: ", agent.session)), "{said}");

    wait_until("the timeout", || agent_list(&home, &cwd)[0]["waiting"] == false);
    assert!(!cwd.join("made-by-the-test").exists(), "an approval that timed out does not run the call");
    let mut late = Client::connect(&agent.socket);
    late.hello();
    let lines = late.until(caught_up);
    let all = serde_json::to_string(&lines).unwrap();
    assert!(all.contains("needs approval"), "the model was told it was unavailable: {all}");
}

#[test]
fn a_client_that_attaches_is_told_which_requests_are_pending_and_a_waiting_flag_clears() {
    let home = home_with_key(&tool_model(json!({"command": "echo x > made-by-the-test"})));
    let cwd = mu_testutil::tmp();
    let agent = ask_mode_agent(&home, &cwd, "");
    let mut first = Client::connect(&agent.socket);
    first.hello();
    first.until(caught_up);
    first.command(AgentCommand::Prompt("go".into()));
    first.until(|v| v["event"].get("Approve").is_some());
    first.send(&ClientLine::Detach);
    wait_until("the waiting flag", || agent_list(&home, &cwd)[0]["waiting"] == true);

    let mut second = Client::connect(&agent.socket);
    second.hello();
    let lines = second.until(caught_up);
    let pending = lines.iter().find(|v| v["event"].get("Pending").is_some()).expect("the pending ids come first");
    assert_eq!(pending["event"]["Pending"]["ids"], json!([1]));
    assert!(lines.iter().any(|v| v["event"].get("Approve").is_some()));
    wait_until("the flag to clear", || agent_list(&home, &cwd)[0]["waiting"] == false);

    // Answered on this client; the next one learns that nothing is pending any more.
    second.send(&ClientLine::Command(allow()));
    second.until(|v| event_is(v, "TurnEnd"));
    let mut third = Client::connect(&agent.socket);
    third.hello();
    let lines = third.until(caught_up);
    let pending = lines.iter().find(|v| v["event"].get("Pending").is_some()).unwrap();
    assert_eq!(pending["event"]["Pending"]["ids"], json!([]), "a dialog shown for id 1 is closed");
    assert!(cwd.join("made-by-the-test").exists());
}

/// A terminal for the real client: a pty with a reader thread that keeps everything the program drew.
struct Term {
    child: Child,
    writer: std::fs::File,
    out: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Term {
    fn spawn(mut command: Command) -> Term {
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut m, mut s) = (-1, -1);
        let ws = libc::winsize { ws_row: 50, ws_col: 220, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: valid out-pointers; the descriptors are fresh and owned below.
        let (master, slave) = unsafe {
            let r = libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of!(ws).cast_mut(),
            );
            assert_eq!(r, 0);
            (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s))
        };
        command
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: setsid and ioctl are async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            })
        };
        let child = command.spawn().unwrap();
        drop(command);
        let writer = std::fs::File::from(master.try_clone().unwrap());
        let mut reader = std::fs::File::from(master);
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = out.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Term { child, writer, out }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, needle: &str) {
        for _ in 0..300 {
            if self.text().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {needle:?}; the terminal got: {:?}", self.text());
    }

    /// Types into the composer once the UI is drawn, then presses Enter.
    fn command(&mut self, line: &str) {
        self.wait_for("type here");
        self.send(line);
        std::thread::sleep(Duration::from_millis(300));
        self.send("\r");
    }

    fn send(&mut self, bytes: &str) {
        self.writer.write_all(bytes.as_bytes()).unwrap();
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        for _ in 0..200 {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the client did not end; it drew: {}", self.text());
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A client machine (its own home) beside a "remote" one that a link script reaches by running this
/// binary with the remote's home: a plain command, as `ssh host mu attach --stdio ID` would be.
struct Far {
    client: mu_testutil::TempDir,
    remote: mu_testutil::TempDir,
    cwd: mu_testutil::TempDir,
    dir: mu_testutil::TempDir,
    link: PathBuf,
    state_key: std::cell::RefCell<Option<PathBuf>>,
}

impl Far {
    fn new() -> Far {
        let (remote, cwd) = fixture();
        let client = mu_testutil::tmp();
        let dir = mu_testutil::tmp();
        let script = format!(
            r#"#!/bin/sh
PATH=/usr/bin:/bin
echo $$ > '{dir}/connect.pid'
while [ -e '{dir}/hold' ]; do sleep 0.1; done
exec env MU_HOME='{remote}' MU_TEST_HOME='{remote}' HOME='{remote}' PATH='{remote}/empty-bin' '{bin}' attach --stdio "$1"
"#,
            dir = dir.display(),
            remote = remote.display(),
            bin = env!("CARGO_BIN_EXE_mu"),
        );
        let link = dir.join("link");
        std::fs::write(&link, script).unwrap();
        std::fs::set_permissions(&link, std::fs::Permissions::from_mode(0o755)).unwrap();
        Far { client, remote, cwd, dir, link, state_key: Default::default() }
    }

    /// An agent running on the remote.
    fn start(&self, extra: &[&str]) -> Agent {
        Agent::start(&self.remote, &self.cwd, extra)
    }

    /// The UI on the client machine, attached to agent `id` through the link.
    fn mu(&self, id: &str) -> Command {
        let mut c = mu(&self.client, &self.cwd);
        c.arg("--attach").arg(format!("{} {id}", self.link.display()));
        if let Some(k) = self.state_key.borrow().as_ref() {
            c.arg("--state-key").arg(k);
        }
        c
    }

    fn agents(&self) -> Vec<Value> {
        agent_list(&self.remote, &self.cwd)
    }

    fn client_host(&self) -> Option<String> {
        self.agents().first().and_then(|a| a["client"]["host"].as_str()).map(str::to_string)
    }
}

#[test]
fn the_ui_attaches_over_a_command_link_detaches_and_rejoins() {
    let far = Far::new();
    let agent = far.start(&[]);
    let mut term = Term::spawn(far.mu(&agent.session));
    wait_until("the agent to have a client", || far.client_host().is_some());
    term.command("a question");
    term.wait_for("hello from the fake");

    term.command("/detach");
    assert!(term.wait_exit().success(), "{}", term.text());
    wait_until("the client to leave", || far.client_host().is_none());

    let mut again = Term::spawn(far.mu(&agent.session));
    wait_until("the second client", || far.client_host().is_some());
    again.wait_for("hello from the fake");
    assert_eq!(far.agents().len(), 1);
    again.command("/detach");
    assert!(again.wait_exit().success());
}

#[test]
fn a_command_that_is_not_an_agents_stream_is_an_error() {
    let far = Far::new();
    let mut c = mu(&far.client, &far.cwd);
    c.args(["--attach", "echo 'no agent here' >&2; exit 3"]);
    let mut term = Term::spawn(c);
    assert!(!term.wait_exit().success());
    assert!(term.text().contains("no agent here"), "{}", term.text());
    let mut c = mu(&far.client, &far.cwd);
    c.args(["--attach", "echo not-json"]);
    let mut term = Term::spawn(c);
    assert!(!term.wait_exit().success());
}

#[test]
fn a_reconnect_does_not_take_the_session_from_the_client_that_has_it() {
    let far = Far::new();
    let agent = far.start(&[]);
    let id = agent.session.clone();
    let term = Term::spawn(far.mu(&id));
    wait_until("the agent to have a client", || far.client_host().is_some());
    let mine = far.client_host().unwrap();
    term.wait_for("type here");

    // The connection drops, and another client attaches before the reconnect gets through.
    std::fs::write(far.dir.join("hold"), "").unwrap();
    let pid: i32 = std::fs::read_to_string(far.dir.join("connect.pid")).unwrap().trim().parse().unwrap();
    // SAFETY: plain signal to the link's process.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_until("the agent to see the loss", || far.client_host().is_none());
    let mut other = Client::connect(&far.remote.join("run").join(format!("{id}.sock")));
    other.send(&hello_line(WIRE, "alpha", false));
    assert!(other.next().unwrap().get("hello").is_some());
    other.until(caught_up);
    std::fs::remove_file(far.dir.join("hold")).unwrap();

    term.wait_for("alpha has this session");
    assert_eq!(far.client_host().as_deref(), Some("alpha"), "the reconnect left alpha alone (was {mine})");
    other.command(AgentCommand::Prompt("still mine".into()));
    other.until(|v| event_is(v, "TurnEnd"));
}

#[test]
fn attaching_by_hand_to_a_busy_agent_asks_and_takes_over_only_on_t() {
    let far = Far::new();
    let agent = far.start(&[]);
    let mut alpha = Client::connect(&agent.socket);
    alpha.send(&hello_line(WIRE, "alpha", false));
    alpha.next().unwrap();
    alpha.until(caught_up);

    let attach = |far: &Far| Term::spawn(far.mu(&agent.session));
    let mut quits = attach(&far);
    quits.wait_for("alpha has this session since");
    quits.send("q");
    assert!(quits.wait_exit().success());
    assert_eq!(far.client_host().as_deref(), Some("alpha"));

    let mut takes = attach(&far);
    takes.wait_for("[t]ake it over");
    takes.send("t");
    wait_until("the take-over", || far.client_host().is_some_and(|h| h != "alpha"));
    alpha.until(|v| v["event"].get("Detached").is_some());
}

/// An SSH ed25519 identity on disk the way `ssh-keygen` leaves it: the private key and `<private>.pub`.
struct Identity {
    private: PathBuf,
    public: String,
    _dir: mu_testutil::TempDir,
}

fn identity(seed: u8, passphrase: Option<&str>) -> Identity {
    use ssh_key::private::Ed25519Keypair;
    let key = ssh_key::PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
    let public = key.public_key().to_openssh().unwrap();
    let stored = match passphrase {
        Some(p) => key.encrypt(&mut rand_core::OsRng, p).unwrap(),
        None => key,
    };
    let dir = mu_testutil::tmp();
    let private = dir.join("id_ed25519");
    std::fs::write(&private, stored.to_openssh(ssh_key::LineEnding::LF).unwrap().as_bytes()).unwrap();
    std::fs::write(format!("{}.pub", private.display()), format!("{public} test@laptop\n")).unwrap();
    Identity { private, public, _dir: dir }
}

/// The key the client unlocks with: what `--state-key` gets.
fn set_client_key(far: &Far, key: &Path) {
    *far.state_key.borrow_mut() = Some(key.to_path_buf());
}

/// A remote whose agents encrypt their state under an SSH key; `notified` collects what `notify_command` got.
fn ssh_far(key: &Identity) -> Far {
    let far = Far::new();
    set_client_key(&far, &key.private);
    let config =
        format!("[agent]\nstate_key = 'ssh'\nnotify_command = 'echo \"$2\" >> {}/notified'\n", far.dir.display());
    std::fs::write(far.remote.join("config.toml"), config).unwrap();
    far
}

fn attach_to(far: &Far, agent: &Agent) -> Term {
    Term::spawn(far.mu(&agent.session))
}

/// The first attach, which sets the state key up; the agent is left running with no client.
fn first_attach(far: &Far, key: &Identity) -> Agent {
    let agent = far.start(&[]);
    let mut term = attach_to(far, &agent);
    term.wait_for("[y/N]");
    assert!(term.text().contains(&key.public), "the key that would unlock it is shown: {}", term.text());
    assert!(!far.remote.join("keys/state.key").exists(), "nothing is set up before the user agrees");
    term.send("y\r");
    term.wait_for("type here");
    assert!(far.remote.join("keys/state.key").exists());
    assert_eq!(far.agents()[0]["locked"], json!(false));
    term.command("/detach");
    assert!(term.wait_exit().success(), "{}", term.text());
    agent
}

/// The agent dies with no goodbye and a new one takes the session: it starts locked.
fn restart_locked(far: &Far, mut old: Agent) -> Agent {
    old.child.kill().unwrap();
    let _ = old.child.wait();
    wait_until("the agent to be gone", || far.agents().is_empty());
    far.start(&["-c"])
}

fn locked_rows(far: &Far) -> (Value, Value) {
    let rows = far.agents();
    (rows[0]["locked"].clone(), rows[0]["waiting"].clone())
}

#[test]
fn an_agent_with_a_state_key_file_starts_unlocked_and_a_wrong_key_stops_it() {
    let (home, cwd) = fixture();
    let secret = home.join("state-secret");
    std::fs::write(&secret, "ab".repeat(32)).unwrap();
    let config = |path: &Path| format!("[agent]\nstate_key_file = '{}'\n", path.display());
    std::fs::write(home.join("config.toml"), config(&secret)).unwrap();

    let mut agent = Agent::start(&home, &cwd, &[]);
    assert!(home.join("keys/state.key").exists(), "state encryption is on");
    let state = std::fs::read_to_string(home.join("keys/state.key")).unwrap();
    assert!(!state.contains(&"ab".repeat(32)), "the key itself is never stored");
    assert_eq!(locked_rows_at(&home, &cwd), (json!(false), json!(false)));
    // Asked over the wire, the agent says encryption is on and open.
    let mut client = Client::connect(&agent.socket);
    client.hello();
    client.until(caught_up);
    client.send(&ClientLine::Host { id: 7, call: "state_status".into(), args: Value::Null });
    let reply = client.until(|v| v.get("hostReply").is_some()).pop().unwrap();
    assert_eq!(
        reply["hostReply"]["result"]["Ok"],
        json!({"enabled": true, "initialised": true, "locked": false, "kind": "raw"})
    );
    drop(client);
    agent.child.kill().unwrap();
    let _ = agent.child.wait();

    // The same key opens the state again after a restart; another key does not.
    drop(agent);
    let agent = Agent::start(&home, &cwd, &["-c"]);
    assert_eq!(locked_rows_at(&home, &cwd).0, json!(false));
    drop(agent);
    let other = home.join("other-secret");
    std::fs::write(&other, [9u8; 32]).unwrap();
    std::fs::write(home.join("config.toml"), config(&other)).unwrap();
    let o = mu(&home, &cwd)
        .args(["agent", "--model", MODEL, "--permissions", "yolo", "-c"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("does not open this profile's state key"), "{err}");
}

fn locked_rows_at(home: &Path, cwd: &Path) -> (Value, Value) {
    let rows = agent_list(home, cwd);
    (rows[0]["locked"].clone(), rows[0]["waiting"].clone())
}

#[test]
fn the_first_attach_sets_up_an_ssh_state_key_and_a_restarted_agent_is_locked_until_a_recipient_unlocks_it() {
    let key = identity(1, None);
    let far = ssh_far(&key);
    let old = first_attach(&far, &key);
    let state = std::fs::read_to_string(far.remote.join("keys/state.key")).unwrap();
    assert!(!state.contains("PRIVATE KEY") && state.contains("recipients"), "{state}");

    let agent = restart_locked(&far, old);
    assert_eq!(locked_rows(&far), (json!(true), json!(true)), "locked, and waiting for someone to unlock it");
    wait_until("the notify command", || {
        std::fs::read_to_string(far.dir.join("notified")).is_ok_and(|t| t.contains("locked: attach to unlock"))
    });

    // A key that is not a recipient is refused, naming what the agent accepts; the client still attaches.
    let other = identity(2, None);
    set_client_key(&far, &other.private);
    let mut refused = attach_to(&far, &agent);
    let fingerprint =
        serde_json::from_str::<Value>(&state).unwrap()["recipients"][0]["fingerprint"].as_str().unwrap().to_string();
    refused.wait_for("not one of the keys");
    assert!(refused.text().contains(&fingerprint), "{}", refused.text());
    refused.wait_for("type here");
    assert_eq!(locked_rows(&far).0, json!(true));
    refused.command("/detach");
    assert!(refused.wait_exit().success());

    // A key that is not there leaves it locked as well.
    set_client_key(&far, Path::new("/nonexistent/id_ed25519"));
    let mut missing = attach_to(&far, &agent);
    missing.wait_for("cannot be read");
    missing.wait_for("type here");
    assert_eq!(locked_rows(&far).0, json!(true));
    missing.command("/detach");
    assert!(missing.wait_exit().success());

    set_client_key(&far, &key.private);
    let mut unlocks = attach_to(&far, &agent);
    unlocks.wait_for("is unlocked");
    unlocks.wait_for("type here");
    wait_until("the agent to be open", || locked_rows(&far) == (json!(false), json!(false)));
    unlocks.command("/detach");
    assert!(unlocks.wait_exit().success());
}

#[test]
fn an_encrypted_key_asks_for_its_passphrase_and_wrong_ones_leave_the_agent_locked() {
    let key = identity(3, Some("hunter2"));
    let far = ssh_far(&key);
    let old = first_attach(&far, &key);
    let agent = restart_locked(&far, old);

    let mut wrong = attach_to(&far, &agent);
    let asked = |t: &Term| t.text().matches("passphrase for").count();
    for (i, guess) in ["one", "two", "three"].iter().enumerate() {
        wait_until("the passphrase prompt", || asked(&wrong) > i);
        assert!(!wrong.text().contains(guess), "the passphrase is not echoed");
        wrong.send(&format!("{guess}\r"));
    }
    wrong.wait_for("too many wrong passphrases");
    wrong.wait_for("type here");
    assert_eq!(locked_rows(&far).0, json!(true));
    wrong.command("/detach");
    assert!(wrong.wait_exit().success());

    let mut right = attach_to(&far, &agent);
    right.wait_for("passphrase for");
    right.send("hunter2\r");
    right.wait_for("is unlocked");
    right.wait_for("type here");
    assert!(!right.text().contains("hunter2"));
    wait_until("the agent to be open", || locked_rows(&far).0 == json!(false));
    right.command("/detach");
    assert!(right.wait_exit().success());
}

#[test]
fn a_reconnect_to_an_agent_that_restarted_locked_unlocks_it_when_the_key_needs_no_passphrase() {
    let key = identity(4, None);
    let far = ssh_far(&key);
    let old = far.start(&[]);
    let mut term = attach_to(&far, &old);
    term.wait_for("[y/N]");
    term.send("y\r");
    term.wait_for("type here");
    wait_until("the agent to have a client", || far.client_host().is_some());

    let agent = restart_locked(&far, old);
    wait_until("the client to unlock the restarted agent", || {
        far.agents().first().is_some_and(|a| a["locked"] == json!(false) && a["client"]["host"].is_string())
    });
    assert!(!term.text().contains("passphrase for"), "a reconnect never asks");
    // The model the agent could not resolve while locked is set once it is open.
    let mut peek = Client::connect(&agent.socket);
    peek.hello();
    let seen = peek.until(caught_up);
    assert!(seen.iter().any(|v| v.to_string().contains("claude-sonnet-5-5")), "{seen:?}");
}

/// A session source: its rows are listed in the Sessions tab with their host, a row with an `attach`
/// command opens as a nested `mu --attach`, and one with only a `hint` shows the hint.
#[test]
fn a_session_source_lists_rows_that_open_by_their_attach_command_or_show_their_hint() {
    let far = Far::new();
    let agent = far.start(&[]);
    let row = |id: &str, started: u64, extra: Value| {
        let mut v = json!({"id": id, "cwd": "/srv/work", "started": started, "title": format!("title of {id}"), "host": "farhost"});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v.to_string()
    };
    let attach = format!("{} {}", far.link.display(), agent.session);
    let rows = [
        row(&agent.session, 1_700_000_000, json!({"attach": attach, "hint": "unused"})),
        row("hintonly", 1_700_000_100, json!({"hint": "open it with the-launcher-script"})),
    ];
    let source = far.dir.join("source.sh");
    std::fs::write(&source, format!("#!/bin/sh\nPATH=/usr/bin:/bin\ncat <<'EOF'\n{}\nEOF\n", rows.join("\n"))).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        far.client.join("config.toml"),
        format!("[sessions]\nsources = [[{:?}]]\n", source.display().to_string()),
    )
    .unwrap();

    let mut term = Term::spawn(mu(&far.client, &far.cwd));
    term.command("/jobs");
    term.wait_for("farhost");
    term.wait_for("Enter attaches to it");
    // The second row only has a hint.
    term.send("j");
    term.wait_for("open it with the-launcher-script");
    term.send("k");
    term.send("\r");
    wait_until("the nested client to attach", || far.client_host().is_some());
    // The nested UI draws its own composer after the outer one's.
    wait_until("the nested UI", || term.text().matches("type here").count() >= 2);
    term.send("/detach");
    std::thread::sleep(Duration::from_millis(300));
    term.send("\r");
    term.wait_for("back from the attached session");
    wait_until("the nested client to leave", || far.client_host().is_none());
    term.send("q");
}
