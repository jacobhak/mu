//! `mu-remote` against a real `mu` and a fake adapter that reaches a "remote" (a second home) by running
//! this binary's `bridge` with that home: the client starts or joins agents, the stream between is
//! encrypted, keys are pinned, and the state-sealing prompts work through `mu --attach`.
#![allow(clippy::unwrap_used)]

mod common;

use common::{Agent, Pid, Term, agent_list, home_with_key, mu_bin, pid_of, serve, sse, tmp, wait_until};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The wire number of the `mu` under test, from `mu contracts`: the first line of a client names it.
fn wire() -> u64 {
    static WIRE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *WIRE.get_or_init(|| {
        let o = Command::new(mu_bin()).arg("contracts").output().unwrap();
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        v["wire"].as_u64().unwrap()
    })
}

/// A plain client of an agent's socket (what `mu attach --stdio` pipes to).
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

    fn send(&mut self, line: &Value) {
        let _ = writeln!(self.writer, "{line}");
        let _ = self.writer.flush();
    }

    fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"))),
            Err(e) => panic!("reading the agent: {e}"),
        }
    }

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

    fn hello(&mut self, host: &str, take: bool) -> Value {
        self.send(&json!({"hello": {
            "wire": wire(), "version": "test", "reconnect": false,
            "host": host, "client": format!("client-of-{host}"), "take": take,
        }}));
        self.next().expect("the agent's hello")
    }

    fn prompt(&mut self, text: &str) {
        self.send(&json!({"command": {"Command": {"Prompt": {"text": text, "images": [], "command": null}}}}));
    }
}

fn caught_up(v: &Value) -> bool {
    v["event"] == "CaughtUp"
}

fn event_is(v: &Value, name: &str) -> bool {
    let e = &v["event"]["Event"];
    e == name || e.get(name).is_some()
}

/// A client machine (its own homes and a `[remote.far]` config) beside a "remote" one that the fake adapter
/// reaches by running `mu-remote bridge` with the remote's homes. Every call is logged.
struct Far {
    mu: PathBuf,
    client: PathBuf,
    remote: PathBuf,
    remote_state: PathBuf,
    cwd: PathBuf,
    dir: PathBuf,
}

impl Far {
    fn new(trusted: bool) -> Option<Far> {
        let mu = mu_bin();
        let remote = home_with_key(&serve(vec![sse(common::ANSWER), sse(common::ANSWER), sse(common::ANSWER)]));
        let (cwd, client, remote_state, dir) = (tmp(), tmp(), tmp(), tmp());
        std::fs::create_dir_all(client.join("empty-bin")).unwrap();
        let script = format!(
            r#"#!/bin/sh
PATH=/usr/bin:/bin
echo "$MU_REMOTE_NAME|$MU_REMOTE_TRUSTED|$*" >> '{dir}/log'
inner() {{ MU_HOME='{remote}' MU_TEST_HOME='{remote}' HOME='{remote}' PATH='{remote}/empty-bin' '{mu}' "$@"; }}
bridge() {{ MU_REMOTE_HOME='{state}' MU_REMOTE_MU='{mu}' MU_HOME='{remote}' MU_TEST_HOME='{remote}' HOME='{remote}' PATH='{remote}/empty-bin' '{mr}' bridge "$@"; }}
case "$1" in
  version) echo '{{"contract":1}}' ;;
  list) [ -e '{dir}/down' ] && {{ echo 'cannot reach the cluster: no route' >&2; exit 1; }}; inner agent list --json ;;
  start) shift 3; cd '{cwd}' && inner agent --detach --model '{model}' --permissions yolo "$@" ;;
  connect)
    echo $$ > '{dir}/connect.pid'
    while [ -e '{dir}/hold' ]; do sleep 0.1; done
    if [ -e '{dir}/capture' ]; then
      mkfifo '{dir}'/in.$$
      exec 3<&0
      tee -a '{dir}/to-agent' <&3 > '{dir}'/in.$$ &
      bridge "$2" < '{dir}'/in.$$ | tee -a '{dir}/from-agent'
      exit
    fi
    exec env MU_REMOTE_HOME='{state}' MU_REMOTE_MU='{mu}' MU_HOME='{remote}' MU_TEST_HOME='{remote}' HOME='{remote}' PATH='{remote}/empty-bin' '{mr}' bridge "$2" ;;
  *) echo "unknown verb $1" >&2; exit 3 ;;
esac
"#,
            dir = dir.display(),
            remote = remote.display(),
            state = remote_state.display(),
            cwd = cwd.display(),
            mu = mu.display(),
            mr = env!("CARGO_BIN_EXE_mu-remote"),
            model = common::MODEL,
        );
        let adapter = dir.join("adapter");
        std::fs::write(&adapter, script).unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = format!("[remote.far]\nadapter = '{}'\ntrusted = {trusted}\n", adapter.display());
        std::fs::write(client.join("mu-remote.toml"), config).unwrap();
        Some(Far { mu, client, remote, remote_state, cwd, dir })
    }

    /// `mu-remote` on the client machine.
    fn mr(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mu-remote"));
        c.current_dir(&self.cwd)
            .env("MU_TEST_HOME", &self.client)
            .env("MU_HOME", &self.client)
            .env("HOME", &self.client)
            .env("PATH", self.client.join("empty-bin"))
            .env("MU_OMP", self.client.join("no-omp"))
            .env("MU_REMOTE_MU", &self.mu)
            .env("MU_REMOTE_CONFIG", self.config())
            .env("MU_REMOTE_HOME", self.client_state())
            .env_remove("OMP_PROFILE")
            .env_remove("MU_GATEWAY_URL")
            .env_remove("MU_THEME")
            .env_remove("ANTHROPIC_API_KEY");
        c
    }

    fn config(&self) -> PathBuf {
        self.client.join("mu-remote.toml")
    }

    fn client_state(&self) -> PathBuf {
        self.client.join("mu-remote-state")
    }

    /// The user's command: `mu-remote far [args]`.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = self.mr();
        c.arg("far").args(args);
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

    fn first_id(&self) -> String {
        self.agents()[0]["id"].as_str().unwrap().to_string()
    }
}

#[test]
fn a_client_starts_an_agent_on_the_remote_connects_and_later_rejoins_it() {
    let Some(far) = Far::new(true) else { return };
    let mut term = Term::spawn(far.cmd(&[]));
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

    let mut again = Term::spawn(far.cmd(&["-c"]));
    wait_until("the second client", || far.client_host().is_some());
    assert_eq!(far.calls("start"), 1, "-c joined the running agent: {:?}", far.log());
    assert_eq!(far.calls("connect"), 2);
    assert_eq!(far.agents().len(), 1);
    again.command("/detach");
    assert!(again.wait_exit().success());
}

#[test]
fn the_stream_the_adapter_carries_holds_no_plaintext_event() {
    let Some(far) = Far::new(true) else { return };
    std::fs::write(far.dir.join("capture"), "").unwrap();
    let mut term = Term::spawn(far.cmd(&[]));
    wait_until("the agent to have a client", || far.client_host().is_some());
    let _agent = pid_of(&far.remote, &far.first_id());
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
        for line in text.lines() {
            assert!(line.starts_with('"') && line.ends_with('"'), "{way}: a line that is not a sealed one: {line}");
        }
    }
    std::thread::sleep(Duration::from_secs(1));
    term.command("/detach");
    assert!(term.wait_exit().success(), "{}", term.text());
}

#[test]
fn the_bridge_key_is_pinned_on_first_contact_shows_what_key_prints_and_a_change_is_refused() {
    let Some(far) = Far::new(true) else { return };
    let mut term = Term::spawn(far.cmd(&[]));
    wait_until("the agent to have a client", || far.client_host().is_some());
    let id = far.first_id();
    let _agent = pid_of(&far.remote, &id);
    // mu shows the first-contact line as a notice that the catch-up repaint may clear: the pin is the fact.
    wait_until("the pin", || far.client_state().join("keys").join("pins.json").exists());
    term.command("/detach");
    assert!(term.wait_exit().success(), "{}", term.text());

    let pins = far.client_state().join("keys").join("pins.json");
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&pins).unwrap()).unwrap();
    let pinned = stored["far"][&id].as_str().unwrap_or_else(|| panic!("no pin for {id}: {stored}")).to_string();
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.decode(&pinned).unwrap();
    let fingerprint = |key: &[u8]| {
        use sha2::{Digest, Sha256};
        format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(key)))
    };
    let mut bridge_side = far.mr();
    bridge_side.env("MU_REMOTE_HOME", &far.remote_state).arg("key");
    let there = bridge_side.output().unwrap();
    assert!(there.status.success());
    let printed = String::from_utf8_lossy(&there.stdout).trim().to_string();
    assert_eq!(printed, fingerprint(&key), "`mu-remote key` prints what the client pinned");
    let mode = std::fs::metadata(far.remote_state.join("keys").join("bridge.key")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // The bridge now shows another key than the pinned one.
    let other = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    std::fs::write(&pins, json!({"far": {id.clone(): other}}).to_string()).unwrap();
    let mut connect = far.mr();
    connect.args(["connect", "far", &id]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = connect.spawn().unwrap();
    let stdin = child.stdin.take();
    let out = child.wait_with_output().unwrap();
    drop(stdin);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Refusing to connect") && err.contains(&printed), "{err}");
    assert!(far.client_host().is_none(), "the agent never got a client");
}

#[test]
fn resuming_a_session_that_no_agent_has_runs_start_with_it() {
    let Some(far) = Far::new(true) else { return };
    let mut term = Term::spawn(far.cmd(&["--resume", "no-such-session"]));
    wait_until("the start call", || far.calls("start") == 1);
    assert!(far.log().iter().any(|l| l == "far|1|start --kind agent --resume no-such-session"), "{:?}", far.log());
    let _ = term.wait_exit();
}

#[test]
fn a_reconnect_does_not_take_the_session_from_the_client_that_has_it() {
    let Some(far) = Far::new(true) else { return };
    let term = Term::spawn(far.cmd(&[]));
    wait_until("the agent to have a client", || far.client_host().is_some());
    let id = far.first_id();
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
    assert!(other.hello("alpha", false).get("hello").is_some());
    other.until(caught_up);
    std::fs::remove_file(far.dir.join("hold")).unwrap();

    term.wait_for("alpha has this session");
    assert_eq!(far.client_host().as_deref(), Some("alpha"), "the reconnect left alpha alone (was {mine})");
    other.prompt("still mine");
    other.until(|v| event_is(v, "TurnEnd"));
}

#[test]
fn attaching_by_hand_to_a_busy_agent_asks_and_takes_over_only_on_t() {
    let Some(far) = Far::new(true) else { return };
    let agent = Agent::start(&far.remote, &far.cwd, &[]);
    let mut alpha = Client::connect(&agent.socket);
    alpha.hello("alpha", false);
    alpha.until(caught_up);

    let resume = |far: &Far| Term::spawn(far.cmd(&["--resume", &agent.session]));
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
        let Some(far) = Far::new(false) else { return };
        let o = far.cmd(args).stdin(Stdio::null()).output().unwrap();
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
    let Some(far) = Far::new(true) else { return };
    let config = std::fs::read_to_string(far.config()).unwrap();
    std::fs::write(far.config(), format!("default_remote = 'far'\n{config}")).unwrap();
    std::fs::write(far.dir.join("down"), "").unwrap();
    let o = far.mr().stdin(Stdio::null()).output().unwrap();
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("cannot reach the cluster: no route"), "{err}");
    assert_eq!(far.calls("start"), 0);
}

#[test]
fn rows_list_the_agents_of_a_trusted_remote_with_host_hint_and_attach() {
    let Some(far) = Far::new(true) else { return };
    let agent = Agent::start(&far.remote, &far.cwd, &[]);
    let o = far.mr().arg("rows").output().unwrap();
    assert!(o.status.success());
    let rows: Vec<Value> =
        String::from_utf8_lossy(&o.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row["id"], agent.session.as_str());
    assert_eq!(row["host"], "far");
    assert_eq!(row["hint"], format!("mu-remote far --resume {}", agent.session));
    let argv = |k: &str| -> Vec<String> {
        row[k].as_array().unwrap().iter().map(|w| w.as_str().unwrap().to_string()).collect()
    };
    assert_eq!(argv("attach")[1..], ["connect", "far", agent.session.as_str()]);
    assert_eq!(argv("helper")[1..], ["helper", "far", agent.session.as_str()]);
    assert_eq!(argv("stop")[1..], ["stop", "far", agent.session.as_str()]);
    assert_eq!(argv("cancel")[1..], ["cancel", "far", agent.session.as_str()]);
    assert_eq!(argv("notify")[1..], ["notify", "far", agent.session.as_str(), "-"]);

    std::fs::write(far.dir.join("down"), "").unwrap();
    let o = far.mr().arg("rows").output().unwrap();
    assert!(o.status.success() && o.stdout.is_empty(), "an unreachable remote adds no rows");
}

/// An SSH ed25519 identity on disk the way `ssh-keygen` leaves it: the private key and `<private>.pub`.
struct Identity {
    private: PathBuf,
    public: String,
}

fn identity(seed: u8, passphrase: Option<&str>) -> Identity {
    use ssh_key::private::Ed25519Keypair;
    let key = ssh_key::PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
    let public = key.public_key().to_openssh().unwrap();
    let stored = match passphrase {
        Some(p) => key.encrypt(&mut rand_core::OsRng, p).unwrap(),
        None => key,
    };
    let private = tmp().join("id_ed25519");
    std::fs::write(&private, stored.to_openssh(ssh_key::LineEnding::LF).unwrap().as_bytes()).unwrap();
    std::fs::write(format!("{}.pub", private.display()), format!("{public} test@laptop\n")).unwrap();
    Identity { private, public }
}

/// The client's `[remote.far] state_key`: the key it unlocks with.
fn set_client_key(far: &Far, key: &Path) {
    let config = std::fs::read_to_string(far.config()).unwrap();
    let kept: Vec<&str> = config.lines().filter(|l| !l.starts_with("state_key")).collect();
    std::fs::write(far.config(), format!("{}\nstate_key = '{}'\n", kept.join("\n"), key.display())).unwrap();
}

/// A remote whose agents encrypt their state under an SSH key; `notified` collects what `notify_command` got.
fn ssh_far(key: &Identity) -> Option<Far> {
    let far = Far::new(true)?;
    set_client_key(&far, &key.private);
    let config =
        format!("[agent]\nstate_key = 'ssh'\nnotify_command = 'echo \"$2\" >> {}/notified'\n", far.dir.display());
    std::fs::write(far.remote.join("config.toml"), config).unwrap();
    Some(far)
}

fn attach_continuing(far: &Far) -> Term {
    Term::spawn(far.cmd(&["-c"]))
}

/// The first attach, which sets the state key up; the agent is left running with no client.
fn first_attach(far: &Far, key: &Identity) -> Pid {
    let mut term = Term::spawn(far.cmd(&[]));
    term.wait_for("[y/N]");
    assert!(term.text().contains(&key.public), "the key that would unlock it is shown: {}", term.text());
    assert!(!far.remote.join("keys/state.key").exists(), "nothing is set up before the user agrees");
    term.send("y\r");
    term.wait_for("type here");
    assert!(far.remote.join("keys/state.key").exists());
    assert_eq!(far.agents()[0]["locked"], json!(false));
    let pid = pid_of(&far.remote, &far.first_id());
    term.command("/detach");
    assert!(term.wait_exit().success(), "{}", term.text());
    pid
}

/// The agent dies with no goodbye and a new one takes the session: it starts locked.
fn restart_locked(far: &Far, old: &Pid) -> Agent {
    // SAFETY: plain signal to the agent the test started.
    unsafe { libc::kill(old.0, libc::SIGKILL) };
    wait_until("the agent to be gone", || far.agents().is_empty());
    Agent::start(&far.remote, &far.cwd, &["-c"])
}

fn locked_rows(far: &Far) -> (Value, Value) {
    let rows = far.agents();
    (rows[0]["locked"].clone(), rows[0]["waiting"].clone())
}

#[test]
fn the_first_attach_sets_up_an_ssh_state_key_and_a_restarted_agent_is_locked_until_a_recipient_unlocks_it() {
    let key = identity(1, None);
    let Some(far) = ssh_far(&key) else { return };
    let old = first_attach(&far, &key);
    let state = std::fs::read_to_string(far.remote.join("keys/state.key")).unwrap();
    assert!(!state.contains("PRIVATE KEY") && state.contains("recipients"), "{state}");

    let _agent = restart_locked(&far, &old);
    assert_eq!(locked_rows(&far), (json!(true), json!(true)), "locked, and waiting for someone to unlock it");
    wait_until("the notify command", || {
        std::fs::read_to_string(far.dir.join("notified")).is_ok_and(|t| t.contains("locked: attach to unlock"))
    });

    // A key that is not a recipient is refused, naming what the agent accepts; the client still attaches.
    let other = identity(2, None);
    set_client_key(&far, &other.private);
    let mut refused = attach_continuing(&far);
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
    let mut missing = attach_continuing(&far);
    missing.wait_for("cannot be read");
    missing.wait_for("type here");
    assert_eq!(locked_rows(&far).0, json!(true));
    missing.command("/detach");
    assert!(missing.wait_exit().success());

    set_client_key(&far, &key.private);
    let mut unlocks = attach_continuing(&far);
    unlocks.wait_for("is unlocked");
    unlocks.wait_for("type here");
    wait_until("the agent to be open", || locked_rows(&far) == (json!(false), json!(false)));
    unlocks.command("/detach");
    assert!(unlocks.wait_exit().success());
}

#[test]
fn an_encrypted_key_asks_for_its_passphrase_and_wrong_ones_leave_the_agent_locked() {
    let key = identity(3, Some("hunter2"));
    let Some(far) = ssh_far(&key) else { return };
    let old = first_attach(&far, &key);
    let _agent = restart_locked(&far, &old);

    let mut wrong = attach_continuing(&far);
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

    let mut right = attach_continuing(&far);
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
    let Some(far) = ssh_far(&key) else { return };
    let mut term = Term::spawn(far.cmd(&[]));
    term.wait_for("[y/N]");
    term.send("y\r");
    term.wait_for("type here");
    wait_until("the agent to have a client", || far.client_host().is_some());
    let old = pid_of(&far.remote, &far.first_id());

    let agent = restart_locked(&far, &old);
    wait_until("the client to unlock the restarted agent", || {
        far.agents().first().is_some_and(|a| a["locked"] == json!(false) && a["client"]["host"].is_string())
    });
    assert!(!term.text().contains("passphrase for"), "a reconnect never asks");
    // The model the agent could not resolve while locked is set once it is open.
    let mut peek = Client::connect(&agent.socket);
    peek.hello("peek", true);
    let seen = peek.until(caught_up);
    assert!(seen.iter().any(|v| v.to_string().contains("claude-sonnet-5-5")), "{seen:?}");
}
