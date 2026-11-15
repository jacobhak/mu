//! The adapter contract (version 1): `<adapter> <verb> ...` with `MU_REMOTE_NAME`, `MU_REMOTE_SETTINGS`
//! (JSON) and `MU_REMOTE_TRUSTED` (`0`/`1`) in the environment. Replies are JSON on stdout, errors are
//! stderr lines, and `{"progress":"..."}` stderr lines report progress. `ssh` is built in (`ssh.rs`);
//! any other adapter is a program.

use super::ssh;
use anyhow::{Context, Result, bail};
use mu_base::config::MuRemote;
use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const CONTRACT: u32 = 1;

enum Kind {
    Ssh { host: String, program: String },
    Program(String),
}

pub struct Adapter {
    name: String,
    kind: Kind,
    settings: String,
    trusted: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartAgent {
    pub resume: Option<String>,
    pub continue_latest: bool,
    pub dir: Option<String>,
    pub model: Option<String>,
}

pub enum Start {
    Agent(StartAgent),
    Worker { launch: String, bundle: PathBuf, dir: String },
}

impl Start {
    fn args(&self) -> Vec<String> {
        let mut a = vec!["--kind".to_string()];
        match self {
            Start::Agent(s) => {
                a.push("agent".into());
                if let Some(id) = &s.resume {
                    a.extend(["--resume".into(), id.clone()]);
                } else if s.continue_latest {
                    a.push("-c".into());
                }
                if let Some(d) = &s.dir {
                    a.extend(["--dir".into(), d.clone()]);
                }
                if let Some(m) = &s.model {
                    a.extend(["--model".into(), m.clone()]);
                }
            }
            Start::Worker { launch, bundle, dir } => {
                a.extend(["worker".into(), "--launch".into(), launch.clone()]);
                a.extend(["--bundle".into(), bundle.to_string_lossy().into_owned(), "--dir".into(), dir.clone()]);
            }
        }
        a
    }
}

/// One line of `list`. Only `id` is required; an adapter may add fields.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AgentInfo {
    pub id: String,
    pub cwd: String,
    pub started: i64,
    pub title: Option<String>,
    pub client: Option<ClientInfo>,
    pub waiting: bool,
    /// `agent` or `worker`; absent means agent.
    pub kind: String,
}

impl AgentInfo {
    pub fn is_agent(&self) -> bool {
        self.kind.is_empty() || self.kind == "agent"
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ClientInfo {
    pub host: String,
    pub since: i64,
}

pub fn parse_list(text: &str) -> Result<Vec<AgentInfo>> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let info: AgentInfo = serde_json::from_str(line).with_context(|| format!("list line {}: {line:?}", n + 1))?;
        if info.id.is_empty() {
            bail!("list line {}: no id", n + 1);
        }
        out.push(info);
    }
    Ok(out)
}

/// The id in a `start` reply (`{"id":ID}`; `mu agent`'s own `{"session":ID,...}` is accepted too).
fn parse_started(text: &str) -> Result<String> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v.get("id").or_else(|| v.get("session")).and_then(|i| i.as_str()).map(str::to_string))
        .filter(|id| !id.is_empty())
        .with_context(|| format!("start printed no id: {:?}", text.trim()))
}

fn parse_contract(text: &str) -> Result<u32> {
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).with_context(|| format!("version reply {:?}", text.trim()))?;
    v.get("contract").and_then(|c| c.as_u64()).map(|c| c as u32).context("version reply has no `contract`")
}

/// Spawns `cmd`, retrying `ETXTBSY`: another thread's fork can briefly hold a just-written script open.
fn spawn(cmd: &mut Command) -> std::io::Result<Child> {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            r => return r,
        }
    }
}

fn kill_group(pid: u32) {
    // SAFETY: plain signal to a process group this module created with `process_group(0)`.
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
}

impl Adapter {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn trusted(&self) -> bool {
        self.trusted
    }

    /// The refusal for a main agent on a remote not marked `trusted = true`.
    pub fn untrusted_agent(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "remote {} is not trusted to run a main agent (set trusted = true on a remote you control)",
            self.name
        )
    }

    pub fn from_config(name: &str, r: &MuRemote) -> Result<Self> {
        r.validate(name).map_err(anyhow::Error::msg)?;
        let kind = if r.adapter.trim() == "ssh" {
            Kind::Ssh { host: r.host.clone().unwrap_or_default().trim().to_string(), program: ssh::program() }
        } else if r.adapter.trim() == "pod" {
            Kind::Program(mu_common::paths::mu_home().join("bin/mu-remote-pod").to_string_lossy().into_owned())
        } else {
            Kind::Program(r.adapter.trim().to_string())
        };
        Ok(Self { name: name.to_string(), kind, settings: r.settings_json(), trusted: r.trusted })
    }

    #[cfg(test)]
    fn with_ssh_program(mut self, program: &str) -> Self {
        if let Kind::Ssh { program: p, .. } = &mut self.kind {
            *p = program.to_string();
        }
        self
    }

    /// `remote` builds the command line the ssh adapter runs on the host; a program gets `verb args`.
    fn command(&self, verb: &str, args: &[String], remote: impl FnOnce() -> Result<String>) -> Result<Command> {
        let mut cmd = match &self.kind {
            Kind::Ssh { host, program } => {
                let mut c = Command::new(program);
                c.args(ssh::argv(host, &remote()?));
                c
            }
            Kind::Program(p) => {
                let mut c = Command::new(p);
                c.arg(verb).args(args);
                c
            }
        };
        if !self.trusted {
            super::worker::scrub_env(&mut cmd);
        }
        cmd.env("MU_REMOTE_NAME", &self.name)
            .env("MU_REMOTE_SETTINGS", &self.settings)
            .env("MU_REMOTE_TRUSTED", if self.trusted { "1" } else { "0" });
        Ok(cmd)
    }

    /// Runs `cmd` to its end: stdout goes to `out`, progress lines to `progress`, and any other stderr line
    /// is the error if it fails.
    fn exec(
        &self,
        verb: &str,
        mut cmd: Command,
        timeout: Option<Duration>,
        progress: &mut dyn FnMut(&str),
        out: &mut (dyn Write + Send),
    ) -> Result<()> {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
        let mut child = spawn(&mut cmd).with_context(|| format!("cannot run adapter `{}`", self.program()))?;
        let pid = child.id();
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            bail!("adapter pipes missing");
        };
        let deadline = timeout.map(|t| Instant::now() + t);
        let past = || deadline.is_some_and(|d| Instant::now() >= d);
        let (tx, rx) = mpsc::channel::<String>();
        let mut errors: Vec<String> = Vec::new();
        let mut timed_out = false;
        let mut status = None;
        let copied = std::thread::scope(|s| {
            let copier = s.spawn(move || std::io::copy(&mut { stdout }, out));
            s.spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(line) => match serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|v| v.get("progress").and_then(|p| p.as_str()).map(str::to_string))
                    {
                        Some(p) => progress(&p),
                        None if !line.trim().is_empty() => errors.push(line.trim().to_string()),
                        None => {}
                    },
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if past() {
                    timed_out = true;
                    kill_group(pid);
                    break;
                }
            }
            while status.is_none() && !timed_out {
                match child.try_wait() {
                    Ok(Some(st)) => status = Some(st),
                    Ok(None) if past() => {
                        timed_out = true;
                        kill_group(pid);
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            if timed_out {
                let _ = child.wait();
            }
            copier.join()
        });
        if timed_out {
            bail!("`{verb}` timed out after {}s", timeout.map_or(0, |t| t.as_secs()));
        }
        let status = status.context("adapter status unknown")?;
        if !status.success() {
            let mut msg = errors.join("\n");
            if msg.is_empty() {
                msg = format!("`{verb}` failed: {status}");
            }
            if let (Kind::Ssh { .. }, Some(h)) = (&self.kind, ssh::hint(&msg)) {
                msg.push_str(&format!("\n{h}"));
            }
            bail!("{msg}");
        }
        match copied {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => bail!("`{verb}` output: {e}"),
            Err(_) => bail!("`{verb}` output thread panicked"),
        }
    }

    fn program(&self) -> &str {
        match &self.kind {
            Kind::Ssh { program, .. } | Kind::Program(program) => program,
        }
    }

    fn capture(
        &self,
        verb: &str,
        cmd: Command,
        timeout: Option<Duration>,
        progress: &mut dyn FnMut(&str),
    ) -> Result<String> {
        let mut out = Vec::new();
        self.exec(verb, cmd, timeout, progress, &mut out)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// The contract version the adapter speaks; anything but 1 is refused. `ssh` answers without running.
    pub fn version(&self, timeout: Option<Duration>) -> Result<u32> {
        if matches!(self.kind, Kind::Ssh { .. }) {
            return Ok(CONTRACT);
        }
        let cmd = self.command("version", &[], || unreachable!())?;
        let v = parse_contract(&self.capture("version", cmd, timeout, &mut |_| {})?)?;
        if v != CONTRACT {
            bail!("adapter `{}` speaks contract {v}; this mu speaks {CONTRACT}", self.name);
        }
        Ok(v)
    }

    /// Starts a detached agent or worker and returns its id once it listens.
    pub fn start(&self, what: &Start, progress: &mut dyn FnMut(&str)) -> Result<String> {
        if matches!(what, Start::Agent(_)) && !self.trusted {
            return Err(self.untrusted_agent());
        }
        let cmd = self.command("start", &what.args(), || ssh::start(what))?;
        parse_started(&self.capture("start", cmd, None, progress)?)
    }

    pub fn list(&self, timeout: Option<Duration>) -> Result<Vec<AgentInfo>> {
        let cmd = self.command("list", &[], || Ok(ssh::list()))?;
        parse_list(&self.capture("list", cmd, timeout, &mut |_| {})?)
    }

    /// `connect ID`, not yet spawned: the caller sets the stdio and owns the stream.
    pub fn connect_command(&self, id: &str) -> Result<Command> {
        self.command("connect", &[id.to_string()], || Ok(ssh::connect(id)))
    }

    /// Spawns `connect ID` with piped stdio.
    pub fn connect(&self, id: &str) -> Result<Child> {
        let mut cmd = self.connect_command(id)?;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        spawn(&mut cmd).with_context(|| format!("cannot run adapter `{}`", self.program()))
    }

    /// Writes the worker's result bundle to `out`.
    pub fn fetch(&self, id: &str, out: &mut (dyn Write + Send)) -> Result<()> {
        let cmd =
            self.command("fetch", &[id.to_string()], || bail!("the ssh adapter has no worker results to fetch"))?;
        self.exec("fetch", cmd, None, &mut |_| {}, out)
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        let cmd = self
            .command("stop", &[id.to_string()], || bail!("the ssh adapter cannot stop an agent; use `/quit` in it"))?;
        self.capture("stop", cmd, None, &mut |_| {}).map(drop)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    /// A fake adapter that logs `NAME|SETTINGS|TRUSTED|args` per call and answers each verb.
    pub(crate) fn fake(dir: &std::path::Path, contract: u32) -> PathBuf {
        let log = dir.join("log");
        let script = format!(
            r#"#!/bin/sh
echo "$MU_REMOTE_NAME|$MU_REMOTE_SETTINGS|$MU_REMOTE_TRUSTED|$*" >> '{log}'
case "$1" in
  version) echo '{{"contract":{contract}}}' ;;
  start)
    echo '{{"progress":"scheduling"}}' >&2
    echo '{{"progress":"starting"}}' >&2
    echo '{{"id":"abc123"}}' ;;
  list)
    echo '{{"id":"abc123","cwd":"/w","started":100,"title":null,"client":{{"host":"h","since":150}},"waiting":true,"extra":1}}'
    echo '{{"id":"def456"}}' ;;
  connect) cat ;;
  fetch) printf 'BUNDLE' ;;
  stop) ;;
  *) echo "first problem" >&2; echo "unknown verb $1" >&2; exit 3 ;;
esac
"#,
            log = log.display()
        );
        let p = dir.join("adapter");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn remote(adapter: &std::path::Path, trusted: bool) -> MuRemote {
        let mut settings = toml::Table::new();
        settings.insert("namespace".into(), "ns".into());
        MuRemote { adapter: adapter.to_string_lossy().into_owned(), settings, trusted, ..Default::default() }
    }

    fn log(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("log")).unwrap().lines().map(str::to_string).collect()
    }

    #[test]
    fn every_verb_runs_with_the_env() {
        let dir = mu_testutil::tmp();
        let a = Adapter::from_config("cluster", &remote(&fake(&dir, 1), true)).unwrap();
        assert_eq!(a.version(None).unwrap(), 1);
        let mut seen = Vec::new();
        let start = StartAgent {
            resume: Some("old".into()),
            dir: Some("/w".into()),
            model: Some("p/m".into()),
            ..Default::default()
        };
        assert_eq!(a.start(&Start::Agent(start), &mut |p| seen.push(p.to_string())).unwrap(), "abc123");
        assert_eq!(seen, ["scheduling", "starting"]);
        let w = Start::Worker { launch: "L".into(), bundle: "/b.bundle".into(), dir: "/d".into() };
        a.start(&w, &mut |_| {}).unwrap();
        let agents = a.list(None).unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(
            agents[0],
            AgentInfo {
                id: "abc123".into(),
                cwd: "/w".into(),
                started: 100,
                title: None,
                client: Some(ClientInfo { host: "h".into(), since: 150 }),
                waiting: true,
                kind: String::new(),
            }
        );
        assert_eq!(agents[1].id, "def456");
        let mut fetched = Vec::new();
        a.fetch("abc123", &mut fetched).unwrap();
        assert_eq!(fetched, b"BUNDLE");
        a.stop("abc123").unwrap();
        let mut child = a.connect("abc123").unwrap();
        child.stdin.take().unwrap().write_all(b"hello\n").unwrap();
        let mut echoed = String::new();
        child.stdout.take().unwrap().read_to_string(&mut echoed).unwrap();
        child.wait().unwrap();
        assert_eq!(echoed, "hello\n");

        let settings = r#"{"namespace":"ns"}"#;
        let want = [
            "version",
            "start --kind agent --resume old --dir /w --model p/m",
            "start --kind worker --launch L --bundle /b.bundle --dir /d",
            "list",
            "fetch abc123",
            "stop abc123",
            "connect abc123",
        ];
        let got = log(&dir);
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (line, args) in got.iter().zip(want) {
            assert_eq!(line, &format!("cluster|{settings}|1|{args}"));
        }
    }

    #[test]
    fn untrusted_is_zero_in_the_env() {
        let dir = mu_testutil::tmp();
        let a = Adapter::from_config("far", &remote(&fake(&dir, 1), false)).unwrap();
        a.list(None).unwrap();
        assert_eq!(log(&dir), [r#"far|{"namespace":"ns"}|0|list"#]);
    }

    #[test]
    fn a_main_agent_is_not_started_on_an_untrusted_remote() {
        let dir = mu_testutil::tmp();
        let a = Adapter::from_config("far", &remote(&fake(&dir, 1), false)).unwrap();
        let e = a.start(&Start::Agent(StartAgent::default()), &mut |_| {}).unwrap_err().to_string();
        assert_eq!(e, "remote far is not trusted to run a main agent (set trusted = true on a remote you control)");
        assert!(!dir.join("log").exists(), "the adapter was never called");
        let w = Start::Worker { launch: "L".into(), bundle: "/b".into(), dir: "/d".into() };
        a.start(&w, &mut |_| {}).unwrap();
    }

    #[test]
    fn another_contract_is_refused() {
        let dir = mu_testutil::tmp();
        let a = Adapter::from_config("x", &remote(&fake(&dir, 2), false)).unwrap();
        let e = a.version(None).unwrap_err().to_string();
        assert!(e.contains("contract 2") && e.contains("speaks 1"), "{e}");
    }

    #[test]
    fn stderr_lines_are_the_error_and_progress_is_not() {
        let dir = mu_testutil::tmp();
        let p = fake(&dir, 1);
        let a = Adapter::from_config("x", &remote(&p, false)).unwrap();
        let cmd = a.command("bogus", &[], || unreachable!()).unwrap();
        let e = a.capture("bogus", cmd, None, &mut |_| {}).unwrap_err().to_string();
        assert_eq!(e, "first problem\nunknown verb bogus");
    }

    #[test]
    fn a_missing_program_and_a_timeout_are_errors() {
        let dir = mu_testutil::tmp();
        let missing = remote(&dir.join("nope"), false);
        let e = Adapter::from_config("x", &missing).unwrap().list(None).unwrap_err().to_string();
        assert!(e.contains("cannot run adapter"), "{e}");
        let slow = dir.join("slow");
        std::fs::write(&slow, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = Instant::now();
        let e = Adapter::from_config("x", &remote(&slow, false))
            .unwrap()
            .list(Some(Duration::from_millis(300)))
            .unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn list_tolerates_missing_and_extra_fields_but_not_garbage() {
        let l = parse_list("\n{\"id\":\"a\",\"future\":[1]}\n").unwrap();
        assert_eq!(l, [AgentInfo { id: "a".into(), ..Default::default() }]);
        assert!(parse_list("not json").unwrap_err().to_string().contains("list line 1"));
        assert!(parse_list("{\"cwd\":\"/x\"}").unwrap_err().to_string().contains("no id"));
    }

    #[test]
    fn start_reply_takes_id_or_the_agents_own_session_line() {
        assert_eq!(parse_started("{\"id\":\"a\"}\n").unwrap(), "a");
        assert_eq!(parse_started("{\"session\":\"s\",\"socket\":\"/x\"}").unwrap(), "s");
        assert!(parse_started("ok").is_err());
    }

    fn fake_ssh(dir: &std::path::Path, body: &str) -> String {
        let log = dir.join("argv");
        let p = dir.join("ssh");
        std::fs::write(&p, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{body}\n", log.display())).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn devbox() -> MuRemote {
        MuRemote { adapter: "ssh".into(), host: Some("devbox".into()), ..Default::default() }
    }

    #[test]
    fn ssh_runs_mu_on_the_host_without_prompting() {
        let dir = mu_testutil::tmp();
        let ssh = fake_ssh(
            &dir,
            r#"case "$5" in *list*) echo '{"id":"z"}' ;; *) echo '{"session":"s9","socket":"/x"}' ;; esac"#,
        );
        let trusted = MuRemote { trusted: true, ..devbox() };
        let a = Adapter::from_config("devbox", &trusted).unwrap().with_ssh_program(&ssh);
        assert_eq!(a.version(None).unwrap(), 1);
        let argv = |dir: &std::path::Path| {
            std::fs::read_to_string(dir.join("argv")).unwrap().lines().map(str::to_string).collect::<Vec<_>>()
        };
        let s = StartAgent { dir: Some("/w x".into()), ..Default::default() };
        assert_eq!(a.start(&Start::Agent(s), &mut |_| {}).unwrap(), "s9");
        assert_eq!(argv(&dir), ["-T", "-o", "BatchMode=yes", "devbox", "cd '/w x' && mu agent --detach"]);
        assert_eq!(a.list(None).unwrap()[0].id, "z");
        assert_eq!(argv(&dir)[4], "mu agent list --json");
        let mut c = a.connect("a'b").unwrap();
        c.wait().unwrap();
        assert_eq!(argv(&dir), ["-T", "-o", "BatchMode=yes", "devbox", r"mu attach --stdio 'a'\''b'"]);
        let w = Start::Worker { launch: "l".into(), bundle: "/b".into(), dir: "/d".into() };
        assert!(a.start(&w, &mut |_| {}).unwrap_err().to_string().contains("cannot run workers"));
    }

    #[test]
    fn ssh_password_hosts_fail_with_an_agent_hint() {
        let dir = mu_testutil::tmp();
        let ssh = fake_ssh(&dir, "echo 'devbox: Permission denied (publickey,password).' >&2; exit 255");
        let a = Adapter::from_config("devbox", &devbox()).unwrap().with_ssh_program(&ssh);
        let e = a.list(None).unwrap_err().to_string();
        assert!(e.contains("Permission denied") && e.contains("ssh-agent"), "{e}");
    }
}
