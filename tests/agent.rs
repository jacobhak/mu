//! `mu agent` and `mu attach`: the real binary, a private `MU_HOME` and cwd, a fake Anthropic API, and a
//! scripted client speaking the wire lines over the agent's Unix socket.
use mu_base::events::Command as AgentCommand;
use mu_base::wire::{ClientLine, WIRE, WireCommand, to_line};
use mu_common::secure::{Handshake, Keypair, Opener, Sealer};
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

/// A client of the wire over any pipe: its first hello opens the channel (the version in the clear, the
/// handshake, then the real hello inside); every line after goes through it. A hello of another wire
/// number gets the agent's plain answer, which `next` returns.
struct Client {
    reader: Box<dyn BufRead>,
    writer: Box<dyn Write>,
    chan: Option<(Sealer, Opener)>,
    plain: std::collections::VecDeque<Value>,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        let stream = UnixStream::connect(socket).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Client::over(Box::new(BufReader::new(stream.try_clone().unwrap())), Box::new(stream))
    }

    fn over(reader: Box<dyn BufRead>, writer: Box<dyn Write>) -> Client {
        Client { reader, writer, chan: None, plain: Default::default() }
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
        if self.chan.is_none()
            && let ClientLine::Hello { wire, .. } = line
        {
            let bare = ClientLine::Hello {
                wire: *wire,
                version: "test".into(),
                reconnect: false,
                host: String::new(),
                client: String::new(),
                take: false,
            };
            let mut hs = Handshake::initiator(&Keypair::generate().unwrap()).unwrap();
            self.write_text(&to_line(&bare));
            self.write_text(&hs.write().unwrap());
            let Some(answer) = self.read_raw() else { return };
            if !answer.is_string() {
                self.plain.push_back(answer);
                return;
            }
            hs.read(&answer).unwrap();
            self.write_text(&hs.write().unwrap());
            let (sealer, opener, _) = hs.finish().unwrap();
            self.chan = Some((sealer, opener));
        }
        let (sealer, _) = self.chan.as_mut().expect("a hello opens the channel");
        for wire in sealer.seal(&to_line(line)).unwrap() {
            self.write_text(&wire);
        }
    }

    /// A hello that takes the session from whoever has it.
    fn hello(&mut self) -> Value {
        self.send(&hello_line(WIRE, "test", true));
        self.next().expect("the agent's hello")
    }

    /// The next line, `None` at the end of the stream.
    fn next(&mut self) -> Option<Value> {
        if let Some(v) = self.plain.pop_front() {
            return Some(v);
        }
        loop {
            let raw = self.read_raw()?;
            let Some((_, opener)) = self.chan.as_mut() else { return Some(raw) };
            if let Some(text) = opener.open(&raw).unwrap() {
                return Some(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{text:?}: {e}")));
            }
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
fn a_login_started_by_a_client_writes_the_agents_credentials_not_the_clients() {
    let (home, cwd) = fixture();
    let omp = home.join("login-omp");
    std::fs::create_dir_all(home.join(".omp/agent")).unwrap();
    std::fs::write(&omp, "#!/bin/sh\nprintf '%s' \"$1\" > \"$HOME/.omp/agent/auth.json\"\n").unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let agent = Agent::start_env(&home, &cwd, &[], &[("MU_OMP", omp.as_path())]);
    let mut c = Client::connect(&agent.socket);
    c.hello();
    c.until(caught_up);
    let run = json!({"id": 6, "program": {"Login": {"args": ["x"], "pick": null}}, "cols": 80, "rows": 24});
    c.send(&ClientLine::Host { id: 1, call: "program_run".into(), args: run });
    let done = c.until(|v| v["hostReply"]["id"] == 1);
    assert_eq!(done.last().unwrap()["hostReply"]["result"]["Ok"]["code"], 0);
    let written = home.join(".omp/agent/auth.json");
    assert_eq!(std::fs::read_to_string(written).unwrap(), "login", "the agent's home got the credentials");
    let clients = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    assert_ne!(clients, home);
    assert_ne!(std::fs::read_to_string(clients.join(".omp/agent/auth.json")).ok().as_deref(), Some("login"));
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
    // The nested client opens its own channel to the second agent, through the first one's tunnel.
    let tunnel = |c: &mut Client, line: &str| c.send(&ClientLine::TunnelLine { id: 7, line: line.to_string() });
    let next_tunnelled = |c: &mut Client| -> Value {
        loop {
            let v = c.next().expect("the tunnel carries lines");
            if let Some(line) = v["tunnelLine"]["line"].as_str() {
                return serde_json::from_str(line).unwrap();
            }
        }
    };
    let bare = ClientLine::Hello {
        wire: WIRE,
        version: "test".into(),
        reconnect: false,
        host: String::new(),
        client: String::new(),
        take: false,
    };
    let mut hs = Handshake::initiator(&Keypair::generate().unwrap()).unwrap();
    tunnel(&mut c, &to_line(&bare));
    tunnel(&mut c, &hs.write().unwrap());
    hs.read(&next_tunnelled(&mut c)).unwrap();
    tunnel(&mut c, &hs.write().unwrap());
    let (mut sealer, mut opener, _) = hs.finish().unwrap();
    for w in sealer.seal(&to_line(&hello_line(WIRE, "nested", false))).unwrap() {
        tunnel(&mut c, &w);
    }
    let tunnelled: Vec<Value> = {
        let mut got = Vec::new();
        loop {
            let wire = next_tunnelled(&mut c);
            let Some(text) = opener.open(&wire).unwrap() else { continue };
            let inner: Value = serde_json::from_str(&text).unwrap();
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
    let exit = to_line(&ClientLine::Command(WireCommand::Command(AgentCommand::Exit)));
    for w in sealer.seal(&exit).unwrap() {
        tunnel(&mut c, &w);
    }
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

/// A client machine (its own home, with a `[remote.far]` config) beside a "remote" one that the fake
/// adapter reaches by running this binary with the remote's home. Every call is logged.
struct Far {
    client: PathBuf,
    remote: PathBuf,
    cwd: PathBuf,
    dir: PathBuf,
}

impl Far {
    fn new(trusted: bool, extra_config: &str) -> Far {
        let (remote, cwd) = fixture();
        let client = mu_testutil::tmp();
        let dir = mu_testutil::tmp();
        let script = format!(
            r#"#!/bin/sh
PATH=/usr/bin:/bin
echo "$MU_REMOTE_NAME|$MU_REMOTE_TRUSTED|$*" >> '{dir}/log'
inner() {{ MU_HOME='{remote}' MU_TEST_HOME='{remote}' HOME='{remote}' PATH='{remote}/empty-bin' '{bin}' "$@"; }}
case "$1" in
  version) echo '{{"contract":1}}' ;;
  list) [ -e '{dir}/down' ] && {{ echo 'cannot reach the cluster: no route' >&2; exit 1; }}; inner agent list --json ;;
  start) shift 3; cd '{cwd}' && inner agent --detach --model '{MODEL}' --permissions yolo "$@" ;;
  connect)
    echo $$ > '{dir}/connect.pid'
    while [ -e '{dir}/hold' ]; do sleep 0.1; done
    if [ -e '{dir}/capture' ]; then
      mkfifo '{dir}'/in.$$
      exec 3<&0
      tee -a '{dir}/to-agent' <&3 > '{dir}'/in.$$ &
      inner attach --stdio "$2" < '{dir}'/in.$$ | tee -a '{dir}/from-agent'
      exit
    fi
    exec env MU_HOME='{remote}' MU_TEST_HOME='{remote}' HOME='{remote}' PATH='{remote}/empty-bin' '{bin}' attach --stdio "$2" ;;
  *) echo "unknown verb $1" >&2; exit 3 ;;
esac
"#,
            dir = dir.display(),
            remote = remote.display(),
            cwd = cwd.display(),
            bin = env!("CARGO_BIN_EXE_mu"),
        );
        let adapter = dir.join("adapter");
        std::fs::write(&adapter, script).unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = format!("[remote.far]\nadapter = '{}'\ntrusted = {trusted}\n{extra_config}", adapter.display());
        std::fs::write(client.join("config.toml"), config).unwrap();
        Far { client, remote, cwd, dir }
    }

    fn mu(&self) -> Command {
        let mut c = mu(&self.client, &self.cwd);
        c.arg("-r").arg("far");
        c
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    fn calls(&self, verb: &str) -> usize {
        self.log().iter().filter(|l| l.split('|').nth(2).is_some_and(|a| a.split(' ').next() == Some(verb))).count()
    }

    fn agents(&self) -> Vec<Value> {
        agent_list(&self.remote, &self.cwd)
    }

    fn client_host(&self) -> Option<String> {
        self.agents().first().and_then(|a| a["client"]["host"].as_str()).map(str::to_string)
    }
}

#[test]
fn a_client_starts_an_agent_on_the_remote_connects_and_later_rejoins_it() {
    let far = Far::new(true, "");
    let mut term = Term::spawn(far.mu());
    wait_until("the agent to have a client", || far.client_host().is_some());
    let agents = far.agents();
    assert_eq!(agents.len(), 1, "{agents:?}");
    let id = agents[0]["id"].as_str().unwrap().to_string();
    let _agent = pid_of(&far.remote, &id);
    assert_eq!(far.calls("start"), 1);
    assert!(far.log().iter().any(|l| l == "far|1|start --kind agent"), "{:?}", far.log());
    assert!(far.log().iter().any(|l| l == &format!("far|1|connect {id}")), "{:?}", far.log());

    term.command("/detach");
    assert!(term.wait_exit().success(), "{}", term.text());
    wait_until("the client to leave", || far.client_host().is_none());

    let mut again = Term::spawn({
        let mut c = far.mu();
        c.arg("-c");
        c
    });
    wait_until("the second client", || far.client_host().is_some());
    assert_eq!(far.calls("start"), 1, "-c joined the running agent: {:?}", far.log());
    assert_eq!(far.calls("connect"), 2);
    assert_eq!(far.agents().len(), 1);
    again.command("/detach");
    assert!(again.wait_exit().success());
}

#[test]
fn the_stream_the_adapter_carries_holds_no_plaintext_event() {
    let far = Far::new(true, "");
    std::fs::write(far.dir.join("capture"), "").unwrap();
    let mut term = Term::spawn(far.mu());
    wait_until("the agent to have a client", || far.client_host().is_some());
    let _agent = pid_of(&far.remote, far.agents()[0]["id"].as_str().unwrap());
    term.command("a-recognisable-prompt");
    term.wait_for("hello from the fake");
    let to = std::fs::read_to_string(far.dir.join("to-agent")).unwrap();
    let from = std::fs::read_to_string(far.dir.join("from-agent")).unwrap();
    assert!(to.lines().count() >= 4 && from.lines().count() >= 4, "the capture holds the exchange");
    for (way, text) in [("to the agent", &to), ("from the agent", &from)] {
        for plain in ["a-recognisable-prompt", "hello from the fake", "UserPrompt", "TurnEnd", "CaughtUp", "\"event\""]
        {
            assert!(!text.contains(plain), "{plain} crossed the pipe in the clear {way}");
        }
    }
    // Only the version is in the clear: the first line the client sends.
    let first: Value = serde_json::from_str(to.lines().next().unwrap()).unwrap();
    assert_eq!(first["hello"]["wire"], WIRE);
    assert!(first["hello"]["host"] == "" && first["hello"]["client"] == "", "{first}");
    std::thread::sleep(Duration::from_secs(1));
    term.command("/detach");
    assert!(term.wait_exit().success(), "{}", term.text());
}

#[test]
fn the_agents_key_is_pinned_on_first_contact_shows_what_agent_key_prints_and_a_change_is_refused() {
    let far = Far::new(true, "");
    let mut term = Term::spawn(far.mu());
    wait_until("the agent to have a client", || far.client_host().is_some());
    let id = far.agents()[0]["id"].as_str().unwrap().to_string();
    let _agent = pid_of(&far.remote, &id);
    term.wait_for("first contact with agent");
    term.command("/detach");
    assert!(term.wait_exit().success(), "{}", term.text());

    let pins = far.client.join("keys").join("pins.json");
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&pins).unwrap()).unwrap();
    let pinned = stored["far"][&id].as_str().unwrap_or_else(|| panic!("no pin for {id}: {stored}")).to_string();
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.decode(&pinned).unwrap();
    let there = mu(&far.remote, &far.cwd).args(["agent", "key"]).output().unwrap();
    assert!(there.status.success());
    let printed = String::from_utf8_lossy(&there.stdout).trim().to_string();
    assert_eq!(printed, mu_common::secure::fingerprint(&key), "`mu agent key` prints what the client pinned");
    let again = mu(&far.remote, &far.cwd).args(["agent", "key"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&again.stdout).trim(), printed, "stable");
    let mode = std::fs::metadata(far.remote.join("keys").join("agent.key")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // The agent now shows another key than the pinned one.
    let other = base64::engine::general_purpose::STANDARD.encode(Keypair::generate().unwrap().public());
    std::fs::write(&pins, json!({"far": {id.clone(): other}}).to_string()).unwrap();
    let mut refused = Term::spawn({
        let mut c = far.mu();
        c.arg("-c");
        c
    });
    assert!(!refused.wait_exit().success());
    let drawn = refused.text();
    assert!(drawn.contains("Refusing to connect") && drawn.contains(&mu_common::secure::fingerprint(&key)), "{drawn}");
    assert!(far.client_host().is_none(), "the agent never got a client");
}

#[test]
fn resuming_a_session_that_no_agent_has_runs_start_with_it() {
    let far = Far::new(true, "");
    let mut term = Term::spawn({
        let mut c = far.mu();
        c.args(["--resume", "no-such-session"]);
        c
    });
    wait_until("the start call", || far.calls("start") == 1);
    assert!(far.log().iter().any(|l| l == "far|1|start --kind agent --resume no-such-session"), "{:?}", far.log());
    let _ = term.wait_exit();
}

#[test]
fn a_reconnect_does_not_take_the_session_from_the_client_that_has_it() {
    let far = Far::new(true, "");
    let term = Term::spawn(far.mu());
    wait_until("the agent to have a client", || far.client_host().is_some());
    let id = far.agents()[0]["id"].as_str().unwrap().to_string();
    let _agent = pid_of(&far.remote, &id);
    let mine = far.client_host().unwrap();
    term.wait_for("type here");

    // The connection drops, and another client attaches before the reconnect gets through.
    std::fs::write(far.dir.join("hold"), "").unwrap();
    let pid: i32 = std::fs::read_to_string(far.dir.join("connect.pid")).unwrap().trim().parse().unwrap();
    // SAFETY: plain signal to the adapter's connect process.
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
    let far = Far::new(true, "");
    let agent = Agent::start(&far.remote, &far.cwd, &[]);
    let mut alpha = Client::connect(&agent.socket);
    alpha.send(&hello_line(WIRE, "alpha", false));
    alpha.next().unwrap();
    alpha.until(caught_up);

    let resume = |far: &Far| {
        let mut c = far.mu();
        c.args(["--resume", &agent.session]);
        Term::spawn(c)
    };
    let mut quits = resume(&far);
    quits.wait_for("alpha has this session since");
    quits.send("q");
    assert!(quits.wait_exit().success());
    assert_eq!(far.client_host().as_deref(), Some("alpha"));
    assert_eq!(far.calls("start"), 0, "the agent was joined, not started");

    let mut takes = resume(&far);
    takes.wait_for("[t]ake it over");
    takes.send("t");
    wait_until("the take-over", || far.client_host().is_some_and(|h| h != "alpha"));
    alpha.until(|v| v["event"].get("Detached").is_some());
}

#[test]
fn a_main_agent_is_refused_on_an_untrusted_remote_and_the_adapter_never_starts_one() {
    for args in [&[][..], &["-c"][..], &["--resume", "abc"][..]] {
        let far = Far::new(false, "");
        let o = far.mu().args(args).stdin(Stdio::null()).output().unwrap();
        assert!(!o.status.success());
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("remote far is not trusted to run a main agent (set trusted = true on a remote you control)"),
            "{err}"
        );
        assert_eq!(far.calls("start"), 0, "{:?}", far.log());
        assert_eq!(far.calls("connect"), 0, "{:?}", far.log());
        assert!(far.agents().is_empty());
    }
}

#[test]
fn an_unreachable_default_remote_is_refused_with_the_adapters_error() {
    let far = Far::new(true, "");
    let config = std::fs::read_to_string(far.client.join("config.toml")).unwrap();
    std::fs::write(far.client.join("config.toml"), format!("default_remote = 'far'\n{config}")).unwrap();
    std::fs::write(far.dir.join("down"), "").unwrap();
    let o = mu(&far.client, &far.cwd).stdin(Stdio::null()).output().unwrap();
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("cannot reach the cluster: no route"), "{err}");
    assert_eq!(far.calls("start"), 0);
    assert!(!far.client.join("sessions").exists(), "nothing ran here instead");
}
