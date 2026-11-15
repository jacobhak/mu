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
        Agent::start_env(home, cwd, extra, &[])
    }

    fn start_env(home: &Path, cwd: &Path, extra: &[&str], env: &[(&str, &Path)]) -> Agent {
        let mut command = mu(home, cwd);
        command
            .envs(env.iter().copied())
            .args(["agent", "--model", MODEL, "--permissions", "yolo"])
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
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let call = json!({"command": "echo from-the-pty", "pty": true});
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

/// A stand-in for omp that prints a prompt, reads one line and exits with 3.
fn fake_omp(dir: &Path) -> PathBuf {
    let path = dir.join("fake-omp");
    std::fs::write(&path, "#!/bin/sh\nprintf 'READY\\n'\nread x\nprintf 'got:%s\\n' \"$x\"\nexit 3\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn a_host_program_runs_on_a_terminal_with_its_output_pushed_and_its_input_taken() {
    use base64::Engine;
    let b64 = &base64::engine::general_purpose::STANDARD;
    let (home, cwd) = fixture();
    let omp = fake_omp(&home);
    let agent = Agent::start_env(&home, &cwd, &[], &[("MU_OMP", omp.as_path())]);
    let mut c = Client::connect(&agent.socket);
    c.hello();
    c.until(caught_up);
    let run = json!({"id": 5, "program": {"McpLogin": {"name": "x"}}, "cols": 80, "rows": 24});
    c.send(&ClientLine::Host { id: 1, call: "program_run".into(), args: run });
    let mut seen = String::new();
    let pushed = |v: &Value| v["pushed"]["name"] == "program_output";
    let text = |v: &Value| {
        String::from_utf8_lossy(&b64.decode(v["pushed"]["data"]["data"].as_str().unwrap()).unwrap()).into_owned()
    };
    while !seen.contains("READY") {
        let lines = c.until(pushed);
        seen.push_str(&text(lines.last().unwrap()));
    }
    let keys = json!({"id": 5, "data": b64.encode(b"hello\n")});
    c.send(&ClientLine::Host { id: 2, call: "program_input".into(), args: keys });
    let mut done = None;
    while done.is_none() {
        let v = c.next().expect("the stream goes on");
        if pushed(&v) {
            seen.push_str(&text(&v));
        } else if v["hostReply"]["id"] == 1 {
            done = Some(v);
        }
    }
    assert!(seen.contains("got:hello"), "the keystrokes reached the program: {seen:?}");
    assert_eq!(done.unwrap()["hostReply"]["result"]["Ok"]["code"], 3);
}

#[test]
fn a_program_ends_with_the_client_that_started_it() {
    let (home, cwd) = fixture();
    let marker = home.join("hung-up");
    let omp = home.join("waiting-omp");
    std::fs::write(
        &omp,
        // Builtins only: the agent's PATH is empty in these tests.
        format!("#!/bin/sh\ntrap ': > \"{}\"; exit 1' HUP\nprintf 'READY\\n'\nread x\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let agent = Agent::start_env(&home, &cwd, &[], &[("MU_OMP", omp.as_path())]);
    let mut c = Client::connect(&agent.socket);
    c.hello();
    c.until(caught_up);
    c.send(&ClientLine::Host {
        id: 1,
        call: "program_run".into(),
        args: json!({"id": 9, "program": {"McpLogin": {"name": "x"}}, "cols": 80, "rows": 24}),
    });
    let mut seen = String::new();
    while !seen.contains("READY") {
        let line = c.until(|v| v["pushed"]["name"] == "program_output").pop().unwrap();
        let data = line["pushed"]["data"]["data"].as_str().unwrap();
        seen.push_str(&String::from_utf8_lossy(
            &base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).unwrap(),
        ));
    }
    // The program waits for a line nobody will send; its client leaves.
    c.send(&ClientLine::Detach);
    for _ in 0..100 {
        if marker.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the program was not hung up when its client left");
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
    let other = Agent::start(&home, &mu_testutil::tmp(), &[]);
    let mut c = Client::connect(&other.socket);
    c.hello();
    c.until(caught_up);
    c.send(&ClientLine::Host { id: 1, call: "take".into(), args: json!({"path": file}) });
    let reply = c.until(|v| v["hostReply"]["id"] == 1).pop().unwrap();
    let session =
        reply["hostReply"]["result"]["Ok"]["session"].as_str().unwrap_or_else(|| panic!("{reply}")).to_string();
    assert_eq!(session, first.session, "the resumed agent has the same session");

    c.send(&ClientLine::TunnelOpen { id: 7, session });
    let hello = to_line(&ClientLine::Hello { wire: WIRE, version: "nested".into() });
    c.send(&ClientLine::TunnelLine { id: 7, line: hello });
    let tunnelled: Vec<Value> = {
        let mut got = Vec::new();
        loop {
            let v = c.next().expect("the tunnel carries lines");
            if let Some(line) = v["tunnelLine"]["line"].as_str() {
                let inner: Value = serde_json::from_str(line).unwrap();
                let done = inner["event"] == "CaughtUp";
                got.push(inner);
                if done {
                    break;
                }
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
    let exit = to_line(&ClientLine::Command(WireCommand::Command(AgentCommand::Exit)));
    c.send(&ClientLine::TunnelLine { id: 7, line: exit });
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
