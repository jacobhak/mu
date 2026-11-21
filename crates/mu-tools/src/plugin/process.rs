//! One plugin process: spawning it in its own group, JSON-RPC requests and replies, the read loop
//! that serves the process's requests, and shutdown.

use super::host::{Kind, PluginHost, Shared, Slot, stop_slot};
use super::manifest::{Lifetime, Manifest};
use super::mcp_runtime::{McpError, mcp_request, refresh_listing};
use mu_common::rpc::{Frame, Framed, Line, Pending, Request, Response, RpcError, kind};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::process::Stdio;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Process groups of every running plugin, so a fatal signal can stop the whole tree (a wrapper
/// like `npx` or `sh -c` leaves grandchildren that PDEATHSIG, which only reaches the leader, misses).
fn live_groups() -> &'static Mutex<Vec<i32>> {
    static G: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();
    G.get_or_init(Default::default)
}

/// SIGKILLs every running plugin's process group, and every process group mu started itself (background
/// jobs, subagents, foreground commands).
pub(crate) fn kill_all_groups() {
    for g in live_groups().lock().expect("live groups lock poisoned").iter() {
        mu_base::process::kill_group(*g, libc::SIGKILL);
    }
    let me = std::process::id() as i32;
    for g in child_groups(me) {
        mu_base::process::kill_group(g, libc::SIGKILL);
    }
    // A direct child in mu's own group cannot be reached by group; kill its tree.
    // SAFETY: plain getpgid(2) on an integer pid.
    let own = unsafe { libc::getpgid(me) };
    for child in direct_children(me) {
        if child.group_id() == Some(own) {
            let _ = child.kill_tree(Some(libc::SIGKILL));
        }
    }
}

fn direct_children(parent: i32) -> Vec<omp_shell::proctree::Process> {
    omp_shell::proctree::Process::from_pid(parent).map(|p| p.children()).unwrap_or_default()
}

/// Process groups of `parent`'s direct children, except `parent`'s own group.
fn child_groups(parent: i32) -> Vec<i32> {
    // SAFETY: plain getpgid(2) on an integer pid.
    let own = unsafe { libc::getpgid(parent) };
    let mut groups: Vec<i32> =
        direct_children(parent).iter().filter_map(|c| c.group_id()).filter(|g| *g > 1 && *g != own).collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

/// For modes without a terminal UI (`-p`, subagents, ACP): on SIGTERM, SIGHUP or SIGINT kill every
/// plugin's and every child's process group, then exit with the signal's shell status. Needs a running
/// `LocalSet`.
pub fn exit_on_signals() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut t), Ok(mut h), Ok(mut i)) =
        (signal(SignalKind::terminate()), signal(SignalKind::hangup()), signal(SignalKind::interrupt()))
    else {
        return;
    };
    tokio::task::spawn_local(async move {
        let code = tokio::select! { _ = t.recv() => 143, _ = h.recv() => 129, _ = i.recv() => 130 };
        kill_all_groups();
        std::process::exit(code);
    });
}

/// The state only an MCP server's process has.
pub(super) struct McpWire {
    /// Requests go out with string ids (`requestIdFormat: "string"`).
    pub(super) string_ids: bool,
    /// The server reported the `tools` capability.
    pub(super) tools_cap: Cell<bool>,
    /// A tool has been called on this process (it was not started only to list tools).
    pub(super) used: Cell<bool>,
    /// The server's `instructions` from `initialize`.
    pub(super) instructions: RefCell<Option<String>>,
}

pub(super) enum Dialect {
    Native,
    Mcp(McpWire),
}

/// Requests from one plugin served at once. A plugin sends one model call at a time and the odd
/// notification; more than this is a runaway loop, and each request is a task.
const MAX_SERVING: u32 = 64;
/// A plugin's `core/complete` is cut off after this long: a model call that never answers must not
/// hold the plugin's request (and the task serving it) forever.
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(600);

pub(super) struct Proc {
    pub(super) name: String,
    pub(super) pid: u32,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pub(super) pending: Pending,
    pub(super) inflight: Cell<u32>,
    /// Plugin-to-core requests being served now.
    serving: Cell<u32>,
    /// Cancelled when the plugin's stdout closes: nobody is left to read the answer to what it asked.
    gone: CancellationToken,
    pub(super) last: Cell<Instant>,
    pub(super) dead: watch::Sender<bool>,
    pub(super) dialect: Dialect,
}

impl Proc {
    pub(super) fn mcp(&self) -> Option<&McpWire> {
        match &self.dialect {
            Dialect::Mcp(w) => Some(w),
            Dialect::Native => None,
        }
    }

    pub(super) fn tools_cap(&self) -> bool {
        self.mcp().is_some_and(|w| w.tools_cap.get())
    }

    pub(super) fn used(&self) -> bool {
        self.mcp().is_some_and(|w| w.used.get())
    }

    pub(super) fn instructions(&self) -> Option<String> {
        self.mcp().and_then(|w| w.instructions.borrow().clone())
    }
}

fn resolve_command(m: &Manifest) -> std::path::PathBuf {
    let c = &m.command[0];
    if c.contains('/') {
        return m.dir.join(c);
    }
    // `current_exe` may read "... (deleted)" after a reinstall; `sibling_of` copes with that.
    if let Some(sib) = std::env::current_exe().ok().and_then(|e| sibling_of(&e, c)) {
        return sib;
    }
    c.into()
}

/// Counts a plugin request as served until dropped.
struct ServingGuard<'a>(&'a Proc);

impl<'a> ServingGuard<'a> {
    fn enter(proc: &'a Proc) -> Self {
        proc.serving.set(proc.serving.get() + 1);
        Self(proc)
    }
}

impl Drop for ServingGuard<'_> {
    fn drop(&mut self) {
        self.0.serving.set(self.0.serving.get().saturating_sub(1));
    }
}

/// Undoes `Proc::request`'s bookkeeping however the request ends.
struct InflightGuard<'a> {
    proc: &'a Proc,
    id: u64,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.proc.pending.cancel(self.id);
        self.proc.inflight.set(self.proc.inflight.get().saturating_sub(1));
        self.proc.last.set(Instant::now());
    }
}

impl Proc {
    pub(super) async fn write_line(&self, line: &str) -> Result<(), String> {
        let mut g = self.stdin.lock().await;
        let Some(w) = g.as_mut() else { return Err(format!("plugin {} exited", self.name)) };
        let r = async {
            w.write_all(line.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await
        }
        .await;
        r.map_err(|_| format!("plugin {} exited", self.name))
    }

    fn wire_id(&self, id: u64) -> Value {
        match self.mcp() {
            Some(w) if w.string_ids => json!(id.to_string()),
            _ => json!(id),
        }
    }

    pub(super) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, String> {
        self.request_c(method, params, timeout, None).await
    }

    /// A request that ends on a reply, `timeout` or `cancel`. An MCP server is told about the last
    /// two with `notifications/cancelled`.
    pub(super) async fn request_c(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, String> {
        enum End {
            Done(Result<Value, String>),
            Timeout,
            Cancelled,
        }
        let (id, rx) = self.pending.register();
        self.inflight.set(self.inflight.get() + 1);
        self.last.set(Instant::now());
        // Runs on every exit, including this future being dropped (a cancelled call).
        let _guard = InflightGuard { proc: self, id };
        let line =
            serde_json::to_string(&Request::new(Some(self.wire_id(id)), method, params)).expect("request serializes");
        let gone = |r: Result<Result<Value, RpcError>, _>| match r {
            Ok(r) => r.map_err(|e| e.message),
            Err(_) => Err(format!("plugin {} exited", self.name)),
        };
        // The write sits inside the timeout and the cancel race: a plugin that stops reading stdin
        // must not hold the call.
        let exchange = async {
            match self.write_line(&line).await {
                Ok(()) => gone(rx.await),
                Err(e) => Err(e),
            }
        };
        let reply = async {
            match timeout {
                Some(t) => match tokio::time::timeout(t, exchange).await {
                    Ok(r) => End::Done(r),
                    Err(_) => End::Timeout,
                },
                None => End::Done(exchange.await),
            }
        };
        let aborted = async {
            match cancel {
                Some(c) => c.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let end = tokio::select! { e = reply => e, _ = aborted => End::Cancelled };
        let why = match end {
            End::Done(r) => return r,
            End::Timeout => "timed out",
            End::Cancelled => "cancelled",
        };
        if self.mcp().is_some() {
            let n =
                Request::new(None, "notifications/cancelled", json!({"requestId": self.wire_id(id), "reason": why}));
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                self.write_line(&serde_json::to_string(&n).expect("notification serializes")),
            )
            .await;
        }
        Err(match why {
            "cancelled" => "Tool call aborted by user".to_string(),
            _ => format!("plugin {} timed out on {method}", self.name),
        })
    }

    pub(super) async fn shutdown(&self) {
        if self.mcp().is_none() {
            // A request, answered when the plugin is done: stdin stays open meanwhile, so core can still reply
            // to what the plugin asks on its way out (sealing its state through `core/secrets`).
            let _ = self.request("shutdown", json!({}), Some(Duration::from_secs(5))).await;
        }
        let Ok(mut stdin) = tokio::time::timeout(Duration::from_secs(2), self.stdin.lock()).await else {
            // A writer is stuck on a full pipe: the plugin is not reading, so stop its group.
            mu_base::process::terminate_group(self.pid as i32).await;
            return;
        };
        stdin.take();
        drop(stdin);
        let mut rx = self.dead.subscribe();
        let exited = tokio::time::timeout(Duration::from_secs(2), async {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
        if exited.is_err() {
            mu_base::process::terminate_group(self.pid as i32).await;
        }
    }
}

async fn read_loop(
    proc: Rc<Proc>,
    slot: Rc<Slot>,
    shared: Rc<Shared>,
    stdout: tokio::process::ChildStdout,
    mut child: tokio::process::Child,
) {
    let mut input = Framed::new(BufReader::new(stdout));
    while let Some(line) = input.next_line().await {
        let v = match line {
            Line::Value(v) => v,
            Line::Bad(why) => {
                let _ = shared.notices.send(format!("plugin {}: skipped an unreadable line ({why})", proc.name));
                continue;
            }
        };
        match mu_common::rpc::frame_of(v) {
            Some(Frame::Response(r)) => {
                let Some(id) = r.id.as_u64().or_else(|| r.id.as_str().and_then(|s| s.parse().ok())) else { continue };
                proc.pending.resolve(
                    id,
                    match r.error {
                        Some(e) => Err(e),
                        None => Ok(r.result.unwrap_or(Value::Null)),
                    },
                );
            }
            Some(Frame::Request(req)) => {
                let (p, s, sh) = (proc.clone(), slot.clone(), shared.clone());
                tokio::task::spawn_local(async move { handle_plugin_request(p, s, sh, req).await });
            }
            None => {}
        }
    }
    // Stdout is gone (EOF or a read error). Fail what is waiting, forget the process, then make
    // sure it is really gone before reaping it: a plugin can close stdout and keep running.
    proc.gone.cancel();
    proc.pending.fail_all(&format!("plugin {} exited", proc.name));
    slot.detach(&proc);
    proc.stdin.lock().await.take();
    // The plugin runs in its own process group (`process_group(0)`).
    mu_base::process::kill_group(proc.pid as i32, libc::SIGTERM);
    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        mu_base::process::kill_group(proc.pid as i32, libc::SIGKILL);
        let _ = child.wait().await;
    }
    live_groups().lock().expect("live groups lock poisoned").retain(|g| *g != proc.pid as i32);
    let _ = proc.dead.send(true);
}

async fn handle_plugin_request(proc: Rc<Proc>, slot: Rc<Slot>, shared: Rc<Shared>, req: Request) {
    if proc.serving.get() >= MAX_SERVING {
        if let Some(id) = req.id {
            let busy = Response::err(id, -32000, format!("too many concurrent requests (limit {MAX_SERVING})"));
            let _ = proc.write_line(&serde_json::to_string(&busy).expect("response serializes")).await;
        }
        return;
    }
    let _serving = ServingGuard::enter(&proc);
    let p = &req.params;
    let mut err_kind = None;
    let mut not_found = false;
    let result: Result<Value, String> = match slot.as_mcp() {
        Some(m) if req.method == "notifications/tools/list_changed" => {
            // Re-list in the background and rewrite the cache; the change applies next session.
            if m.listed.get() && !m.stale.replace(true) {
                tokio::task::spawn_local(refresh_listing(slot.clone(), proc.clone(), shared.clone()));
            }
            Ok(Value::Null)
        }
        Some(m) => mcp_request(&proc, m, &shared, &req).await.map_err(|e| match e {
            McpError::NotFound(s) => {
                not_found = true;
                s
            }
            McpError::Failed(s) => s,
        }),
        None => match req.method.as_str() {
            "core/complete" => {
                let target = p["model"].as_str().or(p["role"].as_str()).unwrap_or("smol").to_string();
                let providers = shared.providers.borrow().upgrade();
                match providers {
                    Some(pr) => {
                        let call = pr.complete(
                            &target,
                            p["system"].as_str().unwrap_or(""),
                            p["prompt"].as_str().unwrap_or(""),
                            p["maxTokens"].as_u64().unwrap_or(2048) as u32,
                            &proc.gone,
                        );
                        match tokio::time::timeout(COMPLETE_TIMEOUT, call).await {
                            Ok(r) => r.map(|text| json!({"text": text})).map_err(|e| {
                                if e.is::<mu_provider::provider::NoModel>() {
                                    err_kind = Some(kind::NO_MODEL);
                                }
                                e.to_string()
                            }),
                            Err(_) => Err(format!("core/complete timed out after {}s", COMPLETE_TIMEOUT.as_secs())),
                        }
                    }
                    None => {
                        err_kind = Some(kind::NO_MODEL);
                        Err(format!("no model configured for role {target}"))
                    }
                }
            }
            "core/webSearch" if proc.name == "web" => {
                mu_provider::auth::web::plugin_config(&mu_provider::auth::store::Store::new()).await
            }
            "core/webSearch" => Err("core/webSearch is only for the web plugin".into()),
            "core/secrets" => core_secrets(&proc.name, p),
            "ui/notify" => {
                let level = p["level"].as_str().unwrap_or("info");
                let _ =
                    shared.notices.send(format!("[{}] {level}: {}", proc.name, p["message"].as_str().unwrap_or("")));
                Ok(Value::Null)
            }
            m => Err(format!("unknown method {m}")),
        },
    };
    if let Some(id) = req.id {
        let resp = match result {
            Ok(v) => Response::ok(id, v),
            // A token that could not be fetched is a failed request, not a missing method.
            Err(e)
                if matches!(
                    req.method.as_str(),
                    "core/mcpSecret" | "core/webSearch" | "core/complete" | "core/secrets"
                ) && !not_found =>
            {
                Response::err_kind(id, -32603, e, err_kind)
            }
            Err(e) => Response::err(id, -32601, e),
        };
        let _ = proc.write_line(&serde_json::to_string(&resp).expect("response serializes")).await;
    }
}

/// `core/secrets`: a plugin's own namespace in the sealed secret store. The namespace is never the plugin's choice.
fn core_secrets(plugin: &str, p: &Value) -> Result<Value, String> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use mu_common::{plugin_secrets as store, sealed};
    let home = mu_common::paths::profile_home();
    let ns = format!("plugin-{plugin}");
    let name = || p["name"].as_str().ok_or_else(|| "missing name".to_string());
    let e = |e: sealed::SealedError| e.to_string();
    match p["op"].as_str().unwrap_or("") {
        "get" => {
            let v = store::get(&home, &ns, name()?).map_err(e)?;
            Ok(json!({"value": v.map(|b| STANDARD.encode(b))}))
        }
        "put" => {
            let v = p["value"].as_str().ok_or("missing value")?;
            let bytes = STANDARD.decode(v).map_err(|_| "value is not base64".to_string())?;
            store::put(&home, &ns, name()?, &bytes).map_err(e)?;
            Ok(json!({}))
        }
        "delete" => Ok(json!({"deleted": store::delete(&home, &ns, name()?).map_err(e)?})),
        "info" => Ok(json!({"sealed": store::sealed(&home), "unlocked": sealed::is_unlocked()})),
        op => Err(format!("unknown core/secrets op `{op}`")),
    }
}

/// `name` next to `exe`, if it exists there. After a reinstall replaces the binary, Linux reports the
/// running exe as ".../mu (deleted)", which canonicalize rejects; its parent directory is still the
/// right one, so don't require it to resolve.
pub(super) fn sibling_of(exe: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    exe.parent().map(|d| d.join(name)).filter(|p| p.exists())
}

/// Where a first-party binary lives: next to the mu binary, else on PATH.
pub(super) fn sibling_or_path(c: &str) -> std::path::PathBuf {
    std::env::current_exe().ok().and_then(|e| sibling_of(&e, c)).unwrap_or_else(|| c.into())
}

impl PluginHost {
    /// Starts the slot's process if it is not running, and runs its handshake.
    pub(super) async fn ensure(&self, slot: &Rc<Slot>) -> Result<Rc<Proc>, String> {
        let _g = slot.start.lock().await;
        if let Some(p) = slot.proc.borrow().clone() {
            return Ok(p);
        }
        let name = slot.name();
        let log_dir = mu_common::paths::mu_home().join("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join(format!("plugin-{name}.log")))
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
        let mut cmd = match &slot.kind {
            Kind::Mcp(m) => self.mcp_command(m).await?,
            Kind::Native { manifest } => {
                let mut c = Command::new(resolve_command(manifest));
                c.args(&manifest.command[1..]).current_dir(&manifest.dir);
                c
            }
        };
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(log);
        let mut child = mu_base::process::spawn_grouped(&mut cmd, true)
            .map_err(|e| format!("cannot start plugin {name} ({}): {e}", slot.program()))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (dead, _) = watch::channel(false);
        let dialect = match &slot.kind {
            Kind::Native { .. } => Dialect::Native,
            Kind::Mcp(ms) => Dialect::Mcp(McpWire {
                string_ids: ms.server.borrow().string_ids,
                tools_cap: Cell::new(false),
                used: Cell::new(false),
                instructions: RefCell::new(None),
            }),
        };
        let proc = Rc::new(Proc {
            name: name.clone(),
            pid: child.id().unwrap_or(0),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            pending: Pending::default(),
            inflight: Cell::new(0),
            serving: Cell::new(0),
            gone: CancellationToken::new(),
            last: Cell::new(Instant::now()),
            dead,
            dialect,
        });
        live_groups().lock().expect("live groups lock poisoned").push(proc.pid as i32);
        *slot.proc.borrow_mut() = Some(proc.clone());
        tokio::task::spawn_local(read_loop(proc.clone(), slot.clone(), self.shared.clone(), stdout, child));
        let handshake = match &slot.kind {
            Kind::Mcp(ms) => {
                let server = ms.server.borrow().clone();
                self.mcp_handshake(&proc, &server).await
            }
            Kind::Native { .. } => {
                let init = self.shared.init.borrow().clone();
                let params = json!({
                    "protocolVersion": 1,
                    "cwd": init.cwd,
                    "sessionId": init.session_id,
                    "sessionFile": init.session_file,
                    "subagent": init.subagent,
                    "muHome": mu_common::paths::mu_home(),
                    "agentDir": mu_common::paths::omp_agent_dir(),
                });
                proc.request("initialize", params, Some(Duration::from_secs(20))).await.map(|_| ())
            }
        };
        if let Err(e) = handshake {
            stop_slot(slot, &proc).await;
            return Err(match slot.as_mcp() {
                Some(ms) if ms.needs_login.get() => self.login_msg(&ms.name()),
                _ => format!("plugin {name} failed to start: {e}"),
            });
        }
        if let Some(ms) = slot.as_mcp()
            && ms.listed.get()
            && proc.tools_cap()
        {
            tokio::task::spawn_local(refresh_listing(slot.clone(), proc.clone(), self.shared.clone()));
        }
        if let Lifetime::Idle(d) = self.lifetime_of(slot) {
            let p = Rc::downgrade(&proc);
            let slot = slot.clone();
            tokio::task::spawn_local(async move {
                loop {
                    let Some(pr) = p.upgrade() else { return };
                    if *pr.dead.borrow() {
                        return;
                    }
                    let due = pr.last.get() + d;
                    drop(pr);
                    tokio::time::sleep_until(due.into()).await;
                    let Some(pr) = p.upgrade() else { return };
                    if pr.inflight.get() == 0 && pr.last.get().elapsed() >= d {
                        // Detach first: a call arriving during shutdown must start a fresh process.
                        stop_slot(&slot, &pr).await;
                        return;
                    }
                }
            });
        }
        Ok(proc)
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn child_groups_lists_a_child_in_its_own_group() {
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 30"]).process_group(0).spawn().unwrap();
        let pid = child.id() as i32;
        let groups = child_groups(std::process::id() as i32);
        // SAFETY: plain kill(2) on the test's own child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = child.wait();
        assert!(groups.contains(&pid), "{pid} not in {groups:?}");
    }
}
