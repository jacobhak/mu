pub mod manifest;
pub mod mcp;

use crate::provider::Providers;
use crate::tools::output::{ToolDef, ToolOutput};
use manifest::{Lifetime, Manifest};
use mu_common::rpc::{Frame, Framed, Line, Pending, Request, Response};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::process::Stdio;
use std::rc::{Rc, Weak};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Process groups of every running plugin, so a fatal signal can stop the whole tree (a wrapper
/// like `npx` or `sh -c` leaves grandchildren that PDEATHSIG, which only reaches the leader, misses).
fn live_groups() -> &'static Mutex<Vec<i32>> {
    static G: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();
    G.get_or_init(Default::default)
}

/// SIGKILLs every running plugin's process group.
pub fn kill_all_groups() {
    for g in live_groups().lock().unwrap().iter().filter(|g| **g > 1) {
        // SAFETY: each plugin runs in its own process group (`process_group(0)`).
        unsafe { libc::kill(-*g, libc::SIGKILL) };
    }
}

/// For modes without a terminal UI (`-p`, subagents, ACP): on SIGTERM or SIGHUP kill every plugin's
/// process group and exit. Needs a running `LocalSet`.
pub fn exit_on_signals() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut t), Ok(mut h)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else { return };
    tokio::task::spawn_local(async move {
        let n = tokio::select! { _ = t.recv() => 15, _ = h.recv() => 1 };
        kill_all_groups();
        std::process::exit(128 + n);
    });
}

#[derive(Clone, Default)]
pub struct InitParams {
    pub cwd: String,
    pub session_id: String,
    pub session_file: Option<String>,
}

/// State shared with every plugin process's reader task.
struct Shared {
    providers: RefCell<Weak<Providers>>,
    notices: mpsc::UnboundedSender<String>,
    init: RefCell<InitParams>,
    /// The plugin that serves `provider/endpoint`, asked for the auth broker's address.
    gateway: RefCell<Option<Weak<Slot>>>,
    cache_dir: std::path::PathBuf,
    /// A terminal UI is attached, so `/login mcp:<name>` can be offered.
    interactive: Cell<bool>,
}

struct Proc {
    name: String,
    pid: u32,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Pending,
    inflight: Cell<u32>,
    last: Cell<Instant>,
    dead: watch::Sender<bool>,
    /// An MCP server: `mcp` dialect, no `shutdown` request.
    mcp: bool,
    /// Requests go out with string ids (`requestIdFormat: "string"`).
    string_ids: bool,
    /// The server reported the `tools` capability.
    tools_cap: Cell<bool>,
    /// A tool has been called on this process (it was not started only to list tools).
    used: Cell<bool>,
    /// The server's `instructions` from `initialize`.
    instructions: RefCell<Option<String>>,
}

struct Slot {
    /// Hooks for this plugin are skipped until then (it timed out on one).
    skip_hooks_until: Cell<Option<Instant>>,
    manifest: Manifest,
    /// Set for an MCP server: its tools come from the config and the cache, not the manifest.
    mcp: Option<mcp::McpSlot>,
    proc: RefCell<Option<Rc<Proc>>>,
    start: tokio::sync::Mutex<()>,
}

/// What `PluginHost::mcp_sync` changed, by server name.
#[derive(Debug, Default, PartialEq)]
pub struct McpSync {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl McpSync {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// The runtime state of one MCP server.
#[derive(Debug, Clone, Default)]
pub struct McpInfo {
    pub running: bool,
    /// The tool list is known this session.
    pub listed: bool,
    pub failed: bool,
    pub needs_login: bool,
    pub tools: Vec<String>,
}

pub struct PluginHost {
    slots: RefCell<Vec<Rc<Slot>>>,
    shared: Rc<Shared>,
    pub warnings: Vec<String>,
    /// Every advertised MCP tool name, so a later listing can tell a clash.
    seen: RefCell<std::collections::HashSet<String>>,
    /// The lifetime of an MCP server added after startup.
    mcp_lifetime: Lifetime,
    /// Built-in tool names: a plugin or MCP tool may not take one.
    reserved: fn(&str) -> bool,
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

impl Slot {
    /// Forgets `proc` as this slot's process, so the next call starts a fresh one.
    fn detach(&self, proc: &Rc<Proc>) {
        let mut cur = self.proc.borrow_mut();
        if cur.as_ref().is_some_and(|c| Rc::ptr_eq(c, proc)) {
            *cur = None;
        }
    }
}

impl Proc {
    async fn write_line(&self, line: &str) -> Result<(), String> {
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
        if self.string_ids { json!(id.to_string()) } else { json!(id) }
    }

    async fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> Result<Value, String> {
        self.request_c(method, params, timeout, None).await
    }

    /// A request that ends on a reply, `timeout` or `cancel`. An MCP server is told about the last
    /// two with `notifications/cancelled`.
    async fn request_c(
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
        let line = serde_json::to_string(&Request::new(Some(self.wire_id(id)), method, params)).unwrap();
        self.write_line(&line).await?;
        let gone =
            |r: Result<Result<Value, String>, _>| r.unwrap_or_else(|_| Err(format!("plugin {} exited", self.name)));
        let reply = async {
            match timeout {
                Some(t) => match tokio::time::timeout(t, rx).await {
                    Ok(r) => End::Done(gone(r)),
                    Err(_) => End::Timeout,
                },
                None => End::Done(gone(rx.await)),
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
        if self.mcp {
            let n =
                Request::new(None, "notifications/cancelled", json!({"requestId": self.wire_id(id), "reason": why}));
            let _ = self.write_line(&serde_json::to_string(&n).unwrap()).await;
        }
        Err(match why {
            "cancelled" => "Tool call aborted by user".to_string(),
            _ => format!("plugin {} timed out on {method}", self.name),
        })
    }

    async fn shutdown(&self) {
        if !self.mcp {
            let _ = self.write_line(&serde_json::to_string(&Request::new(None, "shutdown", json!({}))).unwrap()).await;
        }
        self.stdin.lock().await.take();
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
            crate::proc::terminate_group(self.pid as i32).await;
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
                        Some(e) => Err(e.message),
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
    proc.pending.fail_all(&format!("plugin {} exited", proc.name));
    slot.detach(&proc);
    proc.stdin.lock().await.take();
    if proc.pid > 1 {
        // SAFETY: the plugin runs in its own process group (`process_group(0)`).
        unsafe { libc::kill(-(proc.pid as i32), libc::SIGTERM) };
    }
    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        if proc.pid > 1 {
            unsafe { libc::kill(-(proc.pid as i32), libc::SIGKILL) };
        }
        let _ = child.wait().await;
    }
    live_groups().lock().unwrap().retain(|g| *g != proc.pid as i32);
    let _ = proc.dead.send(true);
}

/// `file://` URI for an absolute path, percent-encoding everything outside the unreserved set and `/`.
fn file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
/// What an MCP server may ask of the client: `ping`, `roots/list` and notifications; the http
/// bridge also asks for OAuth tokens (`core/mcpToken`).
async fn mcp_request(proc: &Proc, slot: &Slot, shared: &Shared, req: &Request) -> Result<Value, String> {
    let m = slot.mcp.as_ref().expect("an mcp slot");
    match req.method.as_str() {
        "ping" => Ok(json!({})),
        "roots/list" => {
            let cwd = shared.init.borrow().cwd.clone();
            Ok(json!({"roots": [{"uri": file_uri(&cwd), "name": "project"}]}))
        }
        "core/mcpToken" if matches!(m.server.borrow().kind, mcp::config::Kind::Http { .. }) => {
            let server = m.server.borrow().clone();
            let force = req.params["force"].as_bool().unwrap_or(false);
            let ids = mcp::auth::credential_ids(&server, mu_common::paths::profile().as_deref());
            let broker = broker_endpoint(shared).await;
            match mcp::auth::fetch_token(&ids, force, broker.as_ref()).await {
                Some(token) => {
                    m.needs_login.set(false);
                    Ok(json!({"token": token}))
                }
                None => {
                    m.needs_login.set(true);
                    Err(format!("no credential for mcp:{}", server.name))
                }
            }
        }
        n if n.starts_with("notifications/") => {
            use std::io::Write;
            // Server log lines and progress go to the plugin log, not the screen.
            let path = mu_common::paths::mu_home().join("logs").join(format!("plugin-{}.log", proc.name));
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{n} {}", req.params);
            }
            Ok(Value::Null)
        }
        other => Err(format!("method not found: {other}")),
    }
}

/// The auth broker's address and token, only when the gateway plugin is already running (this
/// never starts it) and its daemon is alive.
async fn broker_endpoint(shared: &Shared) -> Option<(String, String)> {
    let slot = shared.gateway.borrow().as_ref()?.upgrade()?;
    let proc = slot.proc.borrow().clone()?;
    let v = proc.request("broker/endpoint", json!({}), Some(Duration::from_secs(5))).await.ok()?;
    Some((v["url"].as_str()?.to_string(), v["token"].as_str()?.to_string()))
}

async fn handle_plugin_request(proc: Rc<Proc>, slot: Rc<Slot>, shared: Rc<Shared>, req: Request) {
    let p = &req.params;
    let result: Result<Value, String> = match req.method.as_str() {
        "notifications/tools/list_changed" if slot.mcp.is_some() => {
            // Re-list in the background and rewrite the cache; the change applies next session.
            if let Some(m) = &slot.mcp
                && m.listed.get()
                && !m.stale.replace(true)
            {
                tokio::task::spawn_local(refresh_listing(slot.clone(), proc.clone(), shared.clone()));
            }
            Ok(Value::Null)
        }
        _ if slot.mcp.is_some() => mcp_request(&proc, &slot, &shared, &req).await,
        "core/complete" => {
            let target = p["model"].as_str().or(p["role"].as_str()).unwrap_or("smol").to_string();
            let providers = shared.providers.borrow().upgrade();
            let text = match providers {
                Some(pr) => tokio::time::timeout(
                    Duration::from_secs(180),
                    pr.complete(
                        &target,
                        p["system"].as_str().unwrap_or(""),
                        p["prompt"].as_str().unwrap_or(""),
                        p["maxTokens"].as_u64().unwrap_or(2048) as u32,
                        &CancellationToken::new(),
                    ),
                )
                .await
                .ok()
                .and_then(|r| r.ok()),
                None => None,
            };
            Ok(json!({"text": text}))
        }
        "ui/notify" => {
            let level = p["level"].as_str().unwrap_or("info");
            let _ = shared.notices.send(format!("[{}] {level}: {}", proc.name, p["message"].as_str().unwrap_or("")));
            Ok(Value::Null)
        }
        m => Err(format!("unknown method {m}")),
    };
    if let Some(id) = req.id {
        let resp = match result {
            Ok(v) => Response::ok(id, v),
            // A token that could not be fetched is a failed request, not a missing method.
            Err(e) if req.method == "core/mcpToken" && !e.starts_with("method not found") => {
                Response::err(id, -32603, e)
            }
            Err(e) => Response::err(id, -32601, e),
        };
        let _ = proc.write_line(&serde_json::to_string(&resp).unwrap()).await;
    }
}

/// `name` next to `exe`, if it exists there. After a reinstall replaces the binary, Linux reports the
/// running exe as ".../mu (deleted)", which canonicalize rejects; its parent directory is still the
/// right one, so don't require it to resolve.
fn sibling_of(exe: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    exe.parent().map(|d| d.join(name)).filter(|p| p.exists())
}

/// Where a first-party binary lives: next to the mu binary, else on PATH.
fn sibling_or_path(c: &str) -> std::path::PathBuf {
    std::env::current_exe().ok().and_then(|e| sibling_of(&e, c)).unwrap_or_else(|| c.into())
}

/// `tools/list` with `nextCursor` paging.
async fn list_tools(proc: &Proc, timeout: Option<Duration>) -> Result<Vec<mcp::cache::RemoteTool>, String> {
    if !proc.tools_cap.get() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..100 {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let v = proc.request("tools/list", params, timeout).await?;
        for t in v["tools"].as_array().into_iter().flatten() {
            if let Some(name) = t["name"].as_str() {
                out.push(mcp::cache::RemoteTool {
                    name: name.into(),
                    description: t["description"].as_str().unwrap_or_default().into(),
                    input_schema: t["inputSchema"].clone(),
                });
            }
        }
        match v["nextCursor"].as_str().filter(|c| !c.is_empty()) {
            Some(c) => cursor = Some(c.into()),
            None => break,
        }
    }
    Ok(out)
}

/// After a real start of an already-listed server: compares its tools with the cache and, if they
/// differ, rewrites the cache. The session keeps the tools it started with.
async fn refresh_listing(slot: Rc<Slot>, proc: Rc<Proc>, shared: Rc<Shared>) {
    let Some(m) = slot.mcp.as_ref() else { return };
    let server = m.server.borrow().clone();
    let listed = list_tools(&proc, Some(mcp::connect_timeout(&server))).await;
    let Ok(new) = listed else {
        m.stale.set(false);
        return;
    };
    let key = |t: &mcp::cache::RemoteTool| {
        (t.name.clone(), t.description.clone(), mcp::convert::normalize_schema(&t.input_schema))
    };
    let mut old: Vec<_> = m.remote_tools().iter().map(key).collect();
    let mut cur: Vec<_> = new.iter().map(key).collect();
    old.sort_by(|a, b| a.0.cmp(&b.0));
    cur.sort_by(|a, b| a.0.cmp(&b.0));
    let instructions = if server.instructions { proc.instructions.borrow().clone() } else { None };
    let old_instructions = m.instructions.borrow().clone();
    if old != cur || instructions != old_instructions {
        mcp::cache::save(&shared.cache_dir, &m.hash.borrow(), &server.name, &new, instructions.as_deref());
        if old != cur {
            let _ = shared.notices.send(format!("mcp:{}: tools changed; this applies next session", server.name));
        }
    }
    m.stale.set(false);
}

impl PluginHost {
    #[cfg(test)]
    pub fn new(
        manifests: Vec<Manifest>,
        warnings: Vec<String>,
        notices: mpsc::UnboundedSender<String>,
        init: InitParams,
    ) -> Rc<Self> {
        Self::new_with_mcp(
            manifests,
            Vec::new(),
            Lifetime::Session,
            mcp::cache::dir(),
            warnings,
            notices,
            init,
            crate::tools::is_builtin,
        )
    }

    /// Native manifests, then MCP servers (each a plugin that only provides tools, advertised
    /// from the cache in `cache_dir` when it has them).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_mcp(
        manifests: Vec<Manifest>,
        servers: Vec<mcp::config::McpServer>,
        mcp_lifetime: Lifetime,
        cache_dir: std::path::PathBuf,
        warnings: Vec<String>,
        notices: mpsc::UnboundedSender<String>,
        init: InitParams,
        reserved: fn(&str) -> bool,
    ) -> Rc<Self> {
        let mut warnings = warnings;
        let mut seen = std::collections::HashSet::new();
        let mut slots: Vec<Rc<Slot>> = manifests
            .into_iter()
            .map(|mut m| {
                m.tools.retain(|t| {
                    let clash = reserved(&t.name) || !seen.insert(t.name.clone());
                    if clash {
                        warnings.push(format!("plugin {}: tool `{}` skipped (name collision)", m.name, t.name));
                    }
                    !clash
                });
                Rc::new(Slot {
                    skip_hooks_until: Cell::new(None),
                    manifest: m,
                    mcp: None,
                    proc: RefCell::new(None),
                    start: tokio::sync::Mutex::new(()),
                })
            })
            .collect();
        for s in servers {
            slots.push(Self::mcp_slot(s, mcp_lifetime, &init.cwd, &cache_dir, &mut seen, &mut warnings, reserved));
        }
        let gateway = slots.iter().find(|s| s.manifest.provider.is_some()).map(Rc::downgrade);
        let shared = Rc::new(Shared {
            providers: RefCell::new(Weak::new()),
            notices,
            init: RefCell::new(init),
            gateway: RefCell::new(gateway),
            cache_dir,
            interactive: Cell::new(false),
        });
        Rc::new(Self { slots: RefCell::new(slots), shared, warnings, seen: RefCell::new(seen), mcp_lifetime, reserved })
    }

    /// A slot for an MCP server, advertising its tools from the cache when there is one.
    fn mcp_slot(
        s: mcp::config::McpServer,
        lifetime: Lifetime,
        cwd: &str,
        cache_dir: &std::path::Path,
        seen: &mut std::collections::HashSet<String>,
        warnings: &mut Vec<String>,
        reserved: fn(&str) -> bool,
    ) -> Rc<Slot> {
        let m = Manifest {
            name: format!("mcp:{}", s.name),
            command: vec![s.name.clone()],
            lifetime,
            hooks: Vec::new(),
            provider: None,
            tools: Vec::new(),
            dir: std::path::PathBuf::from(cwd),
        };
        let ms = mcp::McpSlot::new(s, cwd);
        warnings.extend(ms.load_cache(cache_dir, seen, reserved));
        Rc::new(Slot {
            skip_hooks_until: Cell::new(None),
            manifest: m,
            mcp: Some(ms),
            proc: RefCell::new(None),
            start: tokio::sync::Mutex::new(()),
        })
    }

    pub fn set_providers(&self, p: &Rc<Providers>) {
        *self.shared.providers.borrow_mut() = Rc::downgrade(p);
    }

    pub fn set_session(&self, init: InitParams) {
        *self.shared.init.borrow_mut() = init;
    }

    /// Every tool a slot offers: its manifest's, or an MCP server's listed ones.
    fn specs(slot: &Slot) -> Vec<manifest::ToolSpec> {
        match &slot.mcp {
            Some(m) => m.tools.borrow().iter().map(|t| t.spec.clone()).collect(),
            None => slot.manifest.tools.clone(),
        }
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.slots
            .borrow()
            .iter()
            .flat_map(|s| Self::specs(s))
            .filter_map(|t| match t.schema() {
                Ok(schema) => {
                    Some(ToolDef { name: t.name.clone(), description: t.description.clone(), schema: schema.clone() })
                }
                Err(e) => {
                    let _ = self.shared.notices.send(format!("plugin tool `{}` skipped: {e}", t.name));
                    None
                }
            })
            .collect()
    }

    fn slot_for_tool(&self, name: &str) -> Option<Rc<Slot>> {
        self.slots
            .borrow()
            .iter()
            .find(|s| match &s.mcp {
                Some(m) => m.tools.borrow().iter().any(|t| t.spec.name == name),
                None => s.manifest.tools.iter().any(|t| t.name == name),
            })
            .cloned()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.slot_for_tool(name).is_some()
    }

    /// The tool belongs to an MCP server (as opposed to a native plugin).
    pub fn is_mcp_tool(&self, name: &str) -> bool {
        self.slot_for_tool(name).is_some_and(|s| s.mcp.is_some())
    }

    /// `(name, description, running, extra)` per plugin; an MCP server is `mcp:<name>` with its kind
    /// and lifetime, and its tool count as `extra`.
    pub fn status(&self) -> Vec<(String, String, bool, String)> {
        self.slots
            .borrow()
            .iter()
            .map(|s| {
                let (label, extra) = match &s.mcp {
                    Some(m) => {
                        let login = if m.needs_login.get() { " · needs login" } else { "" };
                        let label =
                            format!("{} · {}{login}", m.server.borrow().kind_label(), s.manifest.lifetime.label());
                        (label, format!("{} tools", m.tools.borrow().len()))
                    }
                    None => (s.manifest.lifetime.label(), String::new()),
                };
                (s.manifest.name.clone(), label, s.proc.borrow().is_some(), extra)
            })
            .collect()
    }

    /// Names of the configured MCP servers.
    pub fn mcp_names(&self) -> Vec<String> {
        self.slots.borrow().iter().filter_map(|s| s.mcp.as_ref().map(|m| m.name())).collect()
    }

    /// The runtime state of a configured MCP server.
    pub fn mcp_info(&self, name: &str) -> Option<McpInfo> {
        let slots = self.slots.borrow();
        let s = slots.iter().find(|s| s.mcp.as_ref().is_some_and(|m| m.name() == name))?;
        let m = s.mcp.as_ref()?;
        Some(McpInfo {
            running: s.proc.borrow().is_some(),
            listed: m.listed.get(),
            failed: m.failed.get(),
            needs_login: m.needs_login.get(),
            tools: m.tools.borrow().iter().map(|t| t.spec.name.clone()).collect(),
        })
    }

    /// Brings the MCP servers in line with `servers`, the enabled ones from the config: a removed
    /// server is dropped, a changed one stops and restarts on its new config, a new one is added.
    /// Tools come from the cache; a server without a cached list is listed by the next
    /// `list_missing_until`. The caller refreshes the agent's tool list when anything changed.
    pub async fn mcp_sync(&self, servers: Vec<mcp::config::McpServer>) -> McpSync {
        let cwd = self.shared.init.borrow().cwd.clone();
        let mut out = McpSync::default();
        let mut warnings = Vec::new();
        let mut stop: Vec<Rc<Slot>> = Vec::new();
        {
            let mut slots = self.slots.borrow_mut();
            let mut seen = self.seen.borrow_mut();
            slots.retain(|s| {
                let Some(m) = &s.mcp else { return true };
                let name = m.name();
                if servers.iter().any(|n| n.name == name) {
                    return true;
                }
                for t in m.tools.borrow().iter() {
                    seen.remove(&t.spec.name);
                }
                stop.push(s.clone());
                out.removed.push(name);
                false
            });
            for server in servers {
                let found = slots.iter().find(|s| s.mcp.as_ref().is_some_and(|m| m.name() == server.name)).cloned();
                let Some(slot) = found else {
                    out.added.push(server.name.clone());
                    let s = Self::mcp_slot(
                        server,
                        self.mcp_lifetime,
                        &cwd,
                        &self.shared.cache_dir,
                        &mut seen,
                        &mut warnings,
                        self.reserved,
                    );
                    slots.push(s);
                    continue;
                };
                let m = slot.mcp.as_ref().expect("an mcp slot");
                let hash = mcp::cache::config_hash(&server, &cwd);
                if hash == *m.hash.borrow() {
                    continue;
                }
                out.changed.push(server.name.clone());
                *m.hash.borrow_mut() = hash;
                *m.server.borrow_mut() = server;
                m.listed.set(false);
                m.failed.set(false);
                m.needs_login.set(false);
                m.stale.set(false);
                *m.instructions.borrow_mut() = None;
                warnings.extend(m.set_tools(&[], &mut seen, self.reserved));
                warnings.extend(m.load_cache(&self.shared.cache_dir, &mut seen, self.reserved));
                stop.push(slot.clone());
            }
        }
        for w in warnings {
            let _ = self.shared.notices.send(w);
        }
        for s in stop {
            let p = s.proc.borrow().clone();
            if let Some(p) = p {
                s.detach(&p);
                p.shutdown().await;
            }
        }
        out
    }

    /// MCP servers that need an OAuth login: configured for OAuth, or a token fetch failed.
    /// `(name, url or kind)`.
    pub fn needs_login(&self) -> Vec<(String, String)> {
        self.slots
            .borrow()
            .iter()
            .filter_map(|s| {
                let m = s.mcp.as_ref()?;
                let sv = m.server.borrow();
                (sv.wants_oauth() || m.needs_login.get())
                    .then(|| (sv.name.clone(), sv.url().unwrap_or(sv.kind_label()).to_string()))
            })
            .collect()
    }

    /// The prompt block with every listed server's instructions, or `None`.
    pub fn mcp_instructions(&self) -> Option<String> {
        mcp::render_instructions(
            self.slots
                .borrow()
                .iter()
                .filter_map(|s| {
                    let m = s.mcp.as_ref()?;
                    Some((m.name(), m.instructions.borrow().clone()?))
                })
                .collect(),
        )
    }

    /// Starts every MCP server whose tool list is not cached and lists its tools, all at once.
    /// A failure becomes a notice and leaves that server out of the session. Does nothing when
    /// everything is cached.
    #[cfg(test)]
    pub async fn list_missing(&self) {
        self.list_missing_until(&CancellationToken::new()).await
    }

    /// As `list_missing`, but gives up when `cancel` fires: the servers being listed are stopped and
    /// stay unlisted, so the next turn tries again.
    pub async fn list_missing_until(&self, cancel: &CancellationToken) {
        let todo: Vec<Rc<Slot>> = self
            .slots
            .borrow()
            .iter()
            .filter(|s| s.mcp.as_ref().is_some_and(|m| !m.listed.get() && !m.failed.get()))
            .cloned()
            .collect();
        if todo.is_empty() {
            return;
        }
        let all = futures_util::future::join_all(todo.iter().map(|s| async move {
            if let Err(e) = self.list_slot(s).await {
                let m = s.mcp.as_ref().expect("an mcp slot");
                m.failed.set(true);
                let msg = if m.needs_login.get() {
                    self.login_msg(&m.name())
                } else {
                    format!("mcp:{}: cannot list tools: {e}", m.name())
                };
                let _ = self.shared.notices.send(msg);
            }
        }));
        tokio::select! {
            _ = all => {}
            _ = cancel.cancelled() => {
                for s in &todo {
                    let p = s.proc.borrow().clone();
                    if let Some(p) = p {
                        s.detach(&p);
                        p.shutdown().await;
                    }
                }
            }
        }
    }

    /// The user-facing text for a server that needs an OAuth login. Only interactive mu can run the flow.
    fn login_msg(&self, name: &str) -> String {
        if self.shared.interactive.get() {
            format!("mcp:{name} needs login: /login mcp:{name}")
        } else {
            format!("mcp:{name} needs login (run /login mcp:{name} in mu)")
        }
    }

    /// Marks this host as serving an interactive terminal (`/login mcp:<name>` is then possible).
    pub fn set_interactive(&self) {
        self.shared.interactive.set(true);
    }

    /// Starts the server, lists its tools and makes them this slot's tools (and the cache's).
    async fn list_slot(&self, slot: &Rc<Slot>) -> Result<usize, String> {
        let m = slot.mcp.as_ref().expect("an mcp slot");
        let server = m.server.borrow().clone();
        // Bounded even when the server's call timeout is 0 (no limit): connecting and listing are not calls.
        let timeout = Some(mcp::connect_timeout(&server));
        let run = async {
            let proc = self.ensure(slot).await?;
            match list_tools(&proc, timeout).await {
                Ok(t) => Ok((proc, t)),
                Err(e) => {
                    slot.detach(&proc);
                    proc.shutdown().await;
                    Err(e)
                }
            }
        };
        let limit = mcp::connect_timeout(&server) + Duration::from_secs(30);
        let (proc, tools) = match tokio::time::timeout(limit, run).await {
            Ok(r) => r?,
            Err(_) => {
                // The dropped future left the process attached to the slot: stop it now.
                let p = slot.proc.borrow().clone();
                if let Some(p) = p {
                    slot.detach(&p);
                    p.shutdown().await;
                }
                return Err(format!("no answer within {}s", limit.as_secs()));
            }
        };
        let instructions = if server.instructions { proc.instructions.borrow().clone() } else { None };
        let warnings = m.set_tools(&tools, &mut self.seen.borrow_mut(), self.reserved);
        for w in warnings {
            let _ = self.shared.notices.send(w);
        }
        *m.instructions.borrow_mut() = instructions.clone();
        m.listed.set(true);
        m.needs_login.set(false);
        mcp::cache::save(&self.shared.cache_dir, &m.hash.borrow(), &server.name, &tools, instructions.as_deref());
        // A server started only to list its tools does not stay resident.
        let (weak, slot) = (Rc::downgrade(&proc), slot.clone());
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if let Some(p) = weak.upgrade()
                && !p.used.get()
                && p.inflight.get() == 0
            {
                slot.detach(&p);
                p.shutdown().await;
            }
        });
        Ok(m.tools.borrow().len())
    }

    /// After an interactive login: reloads the server's config entry, restarts it on the new
    /// credential and lists its tools if the session has none yet. `Ok((tools, changed))`.
    pub async fn after_login(&self, name: &str) -> Result<(usize, bool), String> {
        self.reconnect_slot(name, false).await
    }

    /// Restarts the server on its current config and lists its tools again. `Ok((tools, changed))`.
    pub async fn mcp_reconnect(&self, name: &str) -> Result<(usize, bool), String> {
        self.reconnect_slot(name, true).await
    }

    async fn reconnect_slot(&self, name: &str, force: bool) -> Result<(usize, bool), String> {
        let slot = self
            .slots
            .borrow()
            .iter()
            .find(|s| s.mcp.as_ref().is_some_and(|m| m.name() == name))
            .cloned()
            .ok_or_else(|| format!("no MCP server {name}"))?;
        let m = slot.mcp.as_ref().expect("an mcp slot");
        let cwd = self.shared.init.borrow().cwd.clone();
        let (servers, _) = mcp::config::load(std::path::Path::new(&cwd), &mu_common::paths::omp_agent_dir());
        let mut relist = !m.listed.get();
        if let Some(s) = servers.into_iter().find(|s| s.name == name) {
            let hash = mcp::cache::config_hash(&s, &cwd);
            if hash != *m.hash.borrow() {
                *m.hash.borrow_mut() = hash;
                relist = true;
            }
            *m.server.borrow_mut() = s;
        }
        let running = slot.proc.borrow().clone();
        if let Some(p) = running {
            slot.detach(&p);
            p.shutdown().await;
        }
        m.needs_login.set(false);
        m.failed.set(false);
        let before: Vec<String> = m.tools.borrow().iter().map(|t| t.spec.name.clone()).collect();
        if force || relist || m.tools.borrow().is_empty() {
            self.list_slot(&slot).await?;
        }
        let after: Vec<String> = m.tools.borrow().iter().map(|t| t.spec.name.clone()).collect();
        Ok((after.len(), before != after))
    }

    async fn mcp_command(&self, slot: &Slot) -> Result<Command, String> {
        use mcp::config::Kind;
        let m = slot.mcp.as_ref().expect("an mcp slot");
        let server = m.server.borrow().clone();
        let project = std::path::PathBuf::from(&self.shared.init.borrow().cwd);
        let project = if project.as_os_str().is_empty() { ".".into() } else { project };
        match &server.kind {
            Kind::Stdio { command, args, env, cwd } => {
                let dir = cwd.as_ref().map(|c| project.join(c)).unwrap_or_else(|| project.clone());
                let mut cmd = Command::new(command);
                cmd.args(args).current_dir(dir).envs(mcp::auth::resolve_pairs(env, &project).await);
                let ids = mcp::auth::credential_ids(&server, mu_common::paths::profile().as_deref());
                if server.wants_oauth() && !ids.is_empty() {
                    let broker = broker_endpoint(&self.shared).await;
                    match mcp::auth::fetch_token(&ids, false, broker.as_ref()).await {
                        Some(t) => {
                            cmd.env("OAUTH_ACCESS_TOKEN", t);
                        }
                        None => {
                            m.needs_login.set(true);
                            return Err(self.login_msg(&server.name));
                        }
                    }
                }
                Ok(cmd)
            }
            Kind::Http { url, headers } => {
                let headers = mcp::auth::resolve_pairs(headers, &project).await;
                let headers: serde_json::Map<String, Value> =
                    headers.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
                let mut cmd = Command::new(sibling_or_path("mu-mcp-http"));
                cmd.current_dir(&project)
                    .env("MU_MCP_URL", url)
                    .env("MU_MCP_HEADERS", Value::Object(headers).to_string());
                if server.wants_oauth() {
                    cmd.env("MU_MCP_AUTH", server.auth.clone().unwrap_or_default().to_string());
                }
                Ok(cmd)
            }
        }
    }

    async fn ensure(&self, slot: &Rc<Slot>) -> Result<Rc<Proc>, String> {
        let _g = slot.start.lock().await;
        if let Some(p) = slot.proc.borrow().clone() {
            return Ok(p);
        }
        let m = &slot.manifest;
        let log_dir = mu_common::paths::mu_home().join("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join(format!("plugin-{}.log", m.name)))
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());
        let mut cmd = match &slot.mcp {
            Some(_) => self.mcp_command(slot).await?,
            None => {
                let mut c = Command::new(resolve_command(m));
                c.args(&m.command[1..]).current_dir(&m.dir);
                c
            }
        };
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(log);
        cmd.process_group(0);
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("cannot start plugin {} ({}): {e}", m.name, m.command[0]))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (dead, _) = watch::channel(false);
        let proc = Rc::new(Proc {
            name: m.name.clone(),
            pid: child.id().unwrap_or(0),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            pending: Pending::default(),
            inflight: Cell::new(0),
            last: Cell::new(Instant::now()),
            dead,
            mcp: slot.mcp.is_some(),
            string_ids: slot.mcp.as_ref().is_some_and(|s| s.server.borrow().string_ids),
            tools_cap: Cell::new(false),
            used: Cell::new(false),
            instructions: RefCell::new(None),
        });
        live_groups().lock().unwrap().push(proc.pid as i32);
        *slot.proc.borrow_mut() = Some(proc.clone());
        tokio::task::spawn_local(read_loop(proc.clone(), slot.clone(), self.shared.clone(), stdout, child));
        let handshake = match &slot.mcp {
            Some(ms) => {
                let server = ms.server.borrow().clone();
                self.mcp_handshake(&proc, &server).await
            }
            None => {
                let init = self.shared.init.borrow().clone();
                let params = json!({
                    "protocolVersion": 1,
                    "cwd": init.cwd,
                    "sessionId": init.session_id,
                    "sessionFile": init.session_file,
                    "muHome": mu_common::paths::mu_home(),
                    "agentDir": mu_common::paths::omp_agent_dir(),
                });
                proc.request("initialize", params, Some(Duration::from_secs(20))).await.map(|_| ())
            }
        };
        if let Err(e) = handshake {
            slot.detach(&proc);
            proc.shutdown().await;
            return Err(match &slot.mcp {
                Some(ms) if ms.needs_login.get() => self.login_msg(&ms.name()),
                _ => format!("plugin {} failed to start: {e}", m.name),
            });
        }
        if let Some(ms) = &slot.mcp
            && ms.listed.get()
            && proc.tools_cap.get()
        {
            tokio::task::spawn_local(refresh_listing(slot.clone(), proc.clone(), self.shared.clone()));
        }
        if let Lifetime::Idle(d) = m.lifetime {
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
                        slot.detach(&pr);
                        pr.shutdown().await;
                        return;
                    }
                }
            });
        }
        Ok(proc)
    }

    /// MCP `initialize` and `notifications/initialized`; records the capabilities and instructions.
    async fn mcp_handshake(&self, proc: &Rc<Proc>, server: &mcp::config::McpServer) -> Result<(), String> {
        let params = json!({
            "protocolVersion": mcp::PROTOCOL_VERSION,
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "mu", "version": env!("CARGO_PKG_VERSION")},
        });
        let r = proc.request("initialize", params, Some(mcp::connect_timeout(server))).await?;
        proc.tools_cap.set(r["capabilities"].get("tools").is_some_and(|t| !t.is_null()));
        *proc.instructions.borrow_mut() = r["instructions"].as_str().filter(|s| !s.trim().is_empty()).map(String::from);
        let n = Request::new(None, "notifications/initialized", json!({}));
        proc.write_line(&serde_json::to_string(&n).unwrap()).await
    }

    pub async fn call_tool(&self, name: &str, args: &Value, cancel: &CancellationToken) -> ToolOutput {
        let fail = |e: String| ToolOutput::err(e);
        let Some(slot) = self.slot_for_tool(name) else { return fail(format!("Unknown tool: {name}")) };
        let proc = match self.ensure(&slot).await {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        proc.used.set(true);
        let mcp_call = slot.mcp.as_ref().and_then(|m| {
            let tools = m.tools.borrow();
            let t = tools.iter().find(|t| t.spec.name == name)?;
            let schema = t.spec.schema().clone().unwrap_or_default();
            let server = m.server.borrow();
            Some((
                server.name.clone(),
                t.remote.clone(),
                json!({"name": t.remote, "arguments": mcp::convert::clean_args(args, &schema)}),
                mcp::timeout_for(&server),
            ))
        });
        let res = match &mcp_call {
            Some((_, _, params, timeout)) => proc.request_c("tools/call", params.clone(), *timeout, Some(cancel)).await,
            None => proc.request_c("tool/call", json!({"name": name, "arguments": args}), None, Some(cancel)).await,
        };
        if slot.manifest.lifetime == Lifetime::Call {
            slot.detach(&proc);
            proc.shutdown().await;
        }
        match (res, mcp_call) {
            (Ok(v), Some((server, remote, _, _))) => {
                let c = mcp::convert::convert_result(&server, &remote, &v);
                ToolOutput {
                    text: c.text,
                    is_error: c.is_error,
                    display: None,
                    details: Some(c.details),
                    meta: None,
                    images: c.images,
                }
            }
            (Ok(v), None) => ToolOutput {
                text: v["content"].as_str().unwrap_or_default().into(),
                is_error: v["isError"].as_bool().unwrap_or(false),
                display: v["display"].as_str().map(String::from),
                details: None,
                meta: None,
                images: Vec::new(),
            },
            (Err(e), call) => {
                let msg = match &slot.mcp {
                    Some(m) if m.needs_login.get() => self.login_msg(&m.name()),
                    _ => e,
                };
                let mut r = fail(msg);
                // omp records the same details for a call that failed before the server answered.
                r.details = call.map(
                    |(server, remote, _, _)| json!({"serverName": server, "mcpToolName": remote, "isError": true}),
                );
                r
            }
        }
    }

    /// One request to a named plugin, starting it if needed.
    pub async fn request(&self, plugin: &str, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let slot = self
            .slots
            .borrow()
            .iter()
            .find(|s| s.manifest.name == plugin)
            .cloned()
            .ok_or_else(|| format!("no plugin {plugin}"))?;
        let proc = self.ensure(&slot).await?;
        proc.request(method, params, Some(timeout)).await
    }

    /// Runs `hook/<event>` on every plugin subscribed to `sub` (`event` or `event:key`), all at
    /// once under one deadline, and returns the answers in name order. Failures become notices.
    /// A plugin that misses the deadline is skipped for 5 minutes, with one notice.
    pub async fn hook(&self, sub: &str, params: Value) -> Vec<(String, Value)> {
        const SKIP_AFTER_TIMEOUT: Duration = Duration::from_secs(300);
        let event = sub.split(':').next().unwrap_or(sub);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(if event == "session_end" { 10 } else { 30 });
        let now = Instant::now();
        let slots: Vec<Rc<Slot>> = self
            .slots
            .borrow()
            .iter()
            .filter(|s| s.manifest.hooks.iter().any(|h| h == sub))
            .filter(|s| s.skip_hooks_until.get().is_none_or(|t| now >= t))
            .cloned()
            .collect();
        let method = format!("hook/{event}");
        let futs = slots.iter().map(|slot| {
            let (params, method) = (params.clone(), method.clone());
            async move {
                let run = async {
                    let p = self.ensure(slot).await?;
                    p.request(&method, params, None).await
                };
                tokio::time::timeout_at(deadline, run).await
            }
        });
        let results = futures_util::future::join_all(futs).await;
        let mut out = Vec::new();
        for (slot, r) in slots.into_iter().zip(results) {
            let name = &slot.manifest.name;
            match r {
                Ok(Ok(v)) => out.push((name.clone(), v)),
                Ok(Err(e)) => {
                    let _ = self.shared.notices.send(format!("plugin {name}: hook {event} failed: {e}"));
                }
                Err(_) => {
                    slot.skip_hooks_until.set(Some(Instant::now() + SKIP_AFTER_TIMEOUT));
                    let _ = self
                        .shared
                        .notices
                        .send(format!("plugin {name}: hook {event} timed out; skipping its hooks for 5 minutes"));
                }
            }
        }
        out
    }

    pub async fn provider_endpoint(&self, kind: &str) -> Result<(String, String), String> {
        let slot =
            self.slots.borrow().iter().find(|s| s.manifest.provider.as_deref() == Some(kind)).cloned().ok_or_else(
                || "no gateway: install the gateway plugin (~/.mu/plugins/gateway) or set MU_GATEWAY_URL".to_string(),
            )?;
        let proc = self.ensure(&slot).await?;
        let v = proc
            .request("provider/endpoint", json!({"clientPid": std::process::id()}), Some(Duration::from_secs(45)))
            .await?;
        match (v["baseUrl"].as_str(), v["token"].as_str()) {
            (Some(b), Some(t)) => Ok((b.trim_end_matches('/').into(), t.into())),
            _ => Err("gateway plugin returned no endpoint".into()),
        }
    }

    /// The gateway endpoint `GatewaySource` resolves: `MU_GATEWAY_URL` if set, else the gateway plugin.
    pub fn gateway_start(self: &Rc<Self>) -> crate::provider::endpoint::StartFn {
        let host = self.clone();
        crate::provider::endpoint::start_fn(move || {
            let host = host.clone();
            async move {
                match crate::gateway::env_endpoint() {
                    Some(r) => r.map_err(|e| e.to_string()),
                    None => host.provider_endpoint("gateway").await,
                }
            }
        })
    }

    pub async fn shutdown_all(&self) {
        let slots = self.slots.borrow().clone();
        for s in &slots {
            let p = s.proc.borrow().clone();
            if let Some(p) = p {
                s.detach(&p);
                p.shutdown().await;
            }
        }
    }

    #[cfg(test)]
    fn slot(&self, i: usize) -> Rc<Slot> {
        self.slots.borrow()[i].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::block_on_local as run;

    /// A plugin script: answers `initialize`, then hangs on `tool/call` or exits, per `on_call`.
    fn host(on_call: &str, lifetime: &str) -> (Rc<PluginHost>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("mu-plug-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!(
            r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
    *'"shutdown"'*) exit 0 ;;
    *'"tool/call"'*) {on_call} ;;
  esac
done
"#
        );
        std::fs::write(dir.join("p.sh"), script).unwrap();
        let m = Manifest {
            name: "t".into(),
            command: vec!["sh".into(), "p.sh".into()],
            lifetime: manifest::Lifetime::parse(lifetime).unwrap(),
            hooks: vec![],
            provider: None,
            tools: vec![manifest::ToolSpec::with_schema("slow", json!({}))],
            dir: dir.clone(),
        };
        let (ntx, _nrx) = mpsc::unbounded_channel();
        (PluginHost::new(vec![m], vec![], ntx, Default::default()), dir)
    }

    #[test]
    fn a_cancelled_call_releases_its_bookkeeping() {
        run(async {
            let (h, dir) = host("sleep 100", "session");
            let cancel = CancellationToken::new();
            let c = cancel.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                c.cancel();
            });
            let r = h.call_tool("slow", &json!({}), &cancel).await;
            assert!(r.is_error && r.text.contains("aborted"), "{}", r.text);
            let proc = h.slot(0).proc.borrow().clone().unwrap();
            assert_eq!(proc.inflight.get(), 0);
            assert!(proc.pending.is_empty());
            h.shutdown_all().await;
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn a_plugin_that_exits_mid_call_fails_the_call_and_frees_the_slot() {
        run(async {
            let (h, dir) = host("exit 3", "session");
            let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
            assert!(r.is_error, "{}", r.text);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(h.slot(0).proc.borrow().is_none(), "slot cleared so the next call restarts it");
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn a_call_during_idle_shutdown_gets_a_fresh_process() {
        run(async {
            let (h, dir) =
                host("printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":\"ok\"}}\\n' \"$id\"", "idle:1s");
            let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
            assert_eq!(r.text, "ok");
            let first = h.slot(0).proc.borrow().clone().unwrap().pid;
            tokio::time::sleep(Duration::from_millis(1600)).await;
            let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
            assert_eq!(r.text, "ok", "{}", r.text);
            let second = h.slot(0).proc.borrow().clone().unwrap().pid;
            assert_ne!(first, second);
            h.shutdown_all().await;
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    /// A shell-script MCP server: two-page `tools/list`, and calls that return text, an error and an
    /// image, ping the client, or hang. Every line it receives is appended to `$LOG`.
    const MCP_SERVER: &str = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(printf '%s' "$line" | sed -En 's/.*"id":("[0-9]*"|[0-9]*).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"instructions":"be nice"}}\n' "$id" ;;
    *'"method":"tools/list"'*'"cursor":"c2"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"other","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"$schema":"x","type":"object","properties":{"t":{"type":"string"}}}}],"nextCursor":"c2"}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"hi"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"other"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":true,"content":[{"type":"text","text":"boom"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"pic"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"pinger"'*)
      printf '{"jsonrpc":"2.0","id":98,"method":"ping"}\n{"jsonrpc":"2.0","id":99,"method":"roots/list"}\n'
      read -r r1; read -r r2; printf '%s\n%s\n' "$r1" "$r2" >> "$LOG"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"tokener"'*)
      printf '{"jsonrpc":"2.0","id":97,"method":"core/mcpToken","params":{"force":false}}\n'
      read -r r1; printf '%s\n' "$r1" >> "$LOG"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"done"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"hang"'*) sleep 100 & ;;
  esac
done
"#;

    struct McpFx {
        dir: std::path::PathBuf,
        cache: std::path::PathBuf,
    }

    impl McpFx {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("mu-mcp-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(dir.join("agent")).unwrap();
            std::fs::write(dir.join("server.sh"), MCP_SERVER).unwrap();
            Self { cache: dir.join("cache"), dir }
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.join("log")).unwrap_or_default()
        }

        /// A host with one server `srv`; `extra` is more JSON entry fields, `arg` an extra argument.
        fn host(&self, extra: &str, arg: &str) -> (Rc<PluginHost>, mpsc::UnboundedReceiver<String>) {
            let entry = format!(
                r#"{{"mcpServers":{{"srv":{{"command":"sh","args":["server.sh","{arg}"],"env":{{"LOG":"{}"}}{extra}}}}}}}"#,
                self.dir.join("log").display()
            );
            std::fs::write(self.dir.join("agent/mcp.json"), entry).unwrap();
            let (servers, warnings) = mcp::config::load_with(&self.dir, &self.dir.join("agent"), &|_| None);
            assert!(warnings.is_empty(), "{warnings:?}");
            let (ntx, nrx) = mpsc::unbounded_channel();
            let init = InitParams { cwd: self.dir.to_string_lossy().into(), ..Default::default() };
            (
                PluginHost::new_with_mcp(
                    vec![],
                    servers,
                    manifest::Lifetime::Session,
                    self.cache.clone(),
                    vec![],
                    ntx,
                    init,
                    crate::tools::is_builtin,
                ),
                nrx,
            )
        }
    }

    impl Drop for McpFx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn mcp_cold_start_lists_two_pages_then_warm_start_spawns_nothing() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host("", "");
            assert!(h.tool_defs().is_empty(), "nothing is advertised before the first listing");
            h.list_missing().await;
            let names: Vec<String> = h.tool_defs().into_iter().map(|d| d.name).collect();
            assert_eq!(names, ["mcp__srv_echo", "mcp__srv_other"]);
            assert!(h.tool_defs()[0].schema.get("$schema").is_none());
            assert_eq!(h.mcp_instructions().unwrap(), "## MCP Server Instructions\n\n### srv\nbe nice\n");
            let log = fx.log();
            assert!(log.contains(r#""protocolVersion":"2025-11-25""#) && log.contains("notifications/initialized"));
            assert!(!log.contains(r#""method":"shutdown""#) && !log.contains(r#""protocolVersion":1"#), "{log}");
            assert_eq!(log.matches(r#""method":"initialize""#).count(), 1, "one listing start");
            h.shutdown_all().await;

            // Same config: advertised from the cache, nothing is started.
            let (warm, _n) = fx.host("", "");
            assert_eq!(warm.tool_defs().len(), 2);
            assert!(warm.slot(0).proc.borrow().is_none());
            warm.list_missing().await;
            assert_eq!(fx.log().matches(r#""method":"initialize""#).count(), 1, "no second start");
            assert!(warm.mcp_instructions().is_some());

            // Changed args: another hash, so the cache misses and the server is listed again.
            let (changed, _n) = fx.host("", "--x");
            assert!(changed.tool_defs().is_empty());
            changed.list_missing().await;
            assert_eq!(changed.tool_defs().len(), 2);
            assert_eq!(fx.log().matches(r#""method":"initialize""#).count(), 2);
            changed.shutdown_all().await;
        });
    }

    #[test]
    fn mcp_calls_return_text_errors_and_images_and_answer_server_requests() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host("", "");
            h.list_missing().await;
            let c = CancellationToken::new();
            let r = h.call_tool("mcp__srv_echo", &json!({"t": "", "extra": {}}), &c).await;
            assert_eq!((r.text.as_str(), r.is_error), ("hi", false));
            assert_eq!(r.details.unwrap(), json!({"serverName":"srv","mcpToolName":"echo","isError":false}));
            assert!(fx.log().contains(r#""arguments":{}"#), "empty optional values are dropped: {}", fx.log());
            let r = h.call_tool("mcp__srv_other", &json!({}), &c).await;
            assert_eq!((r.text.as_str(), r.is_error), ("Error: boom", true));
            // `pic` is not in the listing, so only the ones listed can be called.
            assert!(h.call_tool("mcp__srv_pic", &json!({}), &c).await.is_error);
            h.shutdown_all().await;
        });
    }

    #[test]
    fn mcp_server_requests_during_a_call_are_answered() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host("", "");
            h.list_missing().await;
            // The script lists only echo and other; call `pinger` through the remote-name map.
            let slot = h.slot(0);
            let m = slot.mcp.as_ref().unwrap();
            let mut taken = std::collections::HashSet::new();
            let tools: Vec<_> = ["echo", "pinger", "pic"]
                .iter()
                .map(|n| mcp::cache::RemoteTool {
                    name: (*n).into(),
                    description: String::new(),
                    input_schema: json!({}),
                })
                .collect();
            m.set_tools(&tools, &mut taken, crate::tools::is_builtin);
            let r = h.call_tool("mcp__srv_pinger", &json!({}), &CancellationToken::new()).await;
            assert_eq!(r.text, "pong", "{}", r.text);
            let log = fx.log();
            assert!(log.contains(r#""id":98,"result":{}"#), "ping answered: {log}");
            assert!(log.contains(r#""id":99,"result":{"roots":[{"uri":"file://"#), "roots answered: {log}");
            let r = h.call_tool("mcp__srv_pic", &json!({}), &CancellationToken::new()).await;
            assert_eq!(r.images.len(), 1);
            assert_eq!(r.images[0].mime_type, "image/png");
            h.shutdown_all().await;
        });
    }

    #[test]
    fn mcp_token_requests_are_refused_from_stdio_servers() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host("", "");
            h.list_missing().await;
            let slot = h.slot(0);
            let m = slot.mcp.as_ref().unwrap();
            let mut taken = std::collections::HashSet::new();
            let t = [mcp::cache::RemoteTool {
                name: "tokener".into(),
                description: String::new(),
                input_schema: json!({}),
            }];
            m.set_tools(&t, &mut taken, crate::tools::is_builtin);
            let r = h.call_tool("mcp__srv_tokener", &json!({}), &CancellationToken::new()).await;
            assert_eq!(r.text, "done", "{}", r.text);
            let log = fx.log();
            assert!(log.contains(r#""id":97,"error":{"code":-32601"#), "method not found for the server: {log}");
            assert!(h.needs_login().is_empty(), "no token was fetched, so no login state changed");
            h.shutdown_all().await;
        });
    }

    #[test]
    fn cancelling_a_listing_stops_the_server_and_leaves_it_unlisted() {
        run(async {
            let fx = McpFx::new();
            // Never answers `initialize`; `timeout: 0` must not make that wait forever.
            std::fs::write(
                fx.dir.join("agent/mcp.json"),
                r#"{"mcpServers":{"hang":{"command":"sh","args":["-c","sleep 1000"],"timeout":0}}}"#,
            )
            .unwrap();
            let (servers, _) = mcp::config::load_with(&fx.dir, &fx.dir.join("agent"), &|_| None);
            assert_eq!(mcp::connect_timeout(&servers[0]), Duration::from_secs(60), "bounded although timeout is 0");
            let (ntx, _nrx) = mpsc::unbounded_channel();
            let init = InitParams { cwd: fx.dir.to_string_lossy().into(), ..Default::default() };
            let h = PluginHost::new_with_mcp(
                vec![],
                servers,
                manifest::Lifetime::Session,
                fx.cache.clone(),
                vec![],
                ntx,
                init,
                crate::tools::is_builtin,
            );
            let cancel = CancellationToken::new();
            let c = cancel.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                c.cancel();
            });
            let t = Instant::now();
            h.list_missing_until(&cancel).await;
            assert!(t.elapsed() < Duration::from_secs(10), "returned promptly: {:?}", t.elapsed());
            let slot = h.slot(0);
            let m = slot.mcp.as_ref().unwrap();
            assert!(slot.proc.borrow().is_none(), "the server was stopped");
            assert!(!m.listed.get() && !m.failed.get(), "left unlisted so the next turn tries again");
        });
    }

    #[test]
    fn login_hint_differs_for_interactive_and_headless() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host("", "");
            assert_eq!(h.login_msg("x"), "mcp:x needs login (run /login mcp:x in mu)");
            h.set_interactive();
            assert_eq!(h.login_msg("x"), "mcp:x needs login: /login mcp:x");
        });
    }

    #[test]
    fn after_login_lists_a_server_whose_listing_failed() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host("", "");
            let slot = h.slot(0);
            let m = slot.mcp.as_ref().unwrap();
            m.failed.set(true);
            m.needs_login.set(true);
            h.list_missing().await;
            assert!(h.tool_defs().is_empty(), "a failed server is left out of the session");
            assert_eq!(h.after_login("srv").await, Ok((2, true)));
            assert_eq!(h.tool_defs().len(), 2);
            assert!(h.needs_login().is_empty());
            assert!(h.after_login("nope").await.is_err());
            h.shutdown_all().await;
        });
    }

    #[test]
    fn mcp_string_ids_round_trip() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host(r#","requestIdFormat":"string""#, "");
            h.list_missing().await;
            assert_eq!(h.tool_defs().len(), 2, "{}", fx.log());
            let r = h.call_tool("mcp__srv_echo", &json!({}), &CancellationToken::new()).await;
            assert_eq!(r.text, "hi");
            assert!(fx.log().contains(r#""id":"1""#), "{}", fx.log());
            h.shutdown_all().await;
        });
    }

    #[test]
    fn mcp_cancel_sends_notifications_cancelled_and_shutdown_leaves_no_process_group() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host("", "");
            h.list_missing().await;
            let slot = h.slot(0);
            let mut taken = std::collections::HashSet::new();
            let hang =
                [mcp::cache::RemoteTool { name: "hang".into(), description: String::new(), input_schema: json!({}) }];
            slot.mcp.as_ref().unwrap().set_tools(&hang, &mut taken, crate::tools::is_builtin);
            let cancel = CancellationToken::new();
            let c = cancel.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                c.cancel();
            });
            let r = h.call_tool("mcp__srv_hang", &json!({}), &cancel).await;
            assert!(r.is_error && r.text.contains("aborted"), "{}", r.text);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(fx.log().contains(r#""method":"notifications/cancelled""#), "{}", fx.log());
            let pid = slot.proc.borrow().clone().unwrap().pid as i32;
            h.shutdown_all().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            // SAFETY: signal 0 only checks that the group exists.
            assert_eq!(unsafe { libc::kill(-pid, 0) }, -1, "the process group is gone");
        });
    }

    #[test]
    fn mcp_sync_adds_changes_and_removes_servers_live() {
        run(async {
            let fx = McpFx::new();
            let (h, _n) = fx.host("", "");
            h.list_missing().await;
            assert_eq!(h.tool_defs().len(), 2);
            let log = fx.dir.join("log").display().to_string();
            let load = |servers: &[(&str, &str)]| {
                let body: Vec<String> = servers
                    .iter()
                    .map(|(n, a)| {
                        format!(r#""{n}":{{"command":"sh","args":["server.sh","{a}"],"env":{{"LOG":"{log}"}}}}"#)
                    })
                    .collect();
                std::fs::write(fx.dir.join("agent/mcp.json"), format!(r#"{{"mcpServers":{{{}}}}}"#, body.join(",")))
                    .unwrap();
                mcp::config::load_with(&fx.dir, &fx.dir.join("agent"), &|_| None).0
            };
            let inits = || fx.log().matches(r#""method":"initialize""#).count();

            assert!(h.mcp_sync(load(&[("srv", "")])).await.is_empty(), "same config changes nothing");

            let r = h.mcp_sync(load(&[("srv", ""), ("two", "b")])).await;
            assert_eq!(r, McpSync { added: vec!["two".into()], ..Default::default() });
            assert_eq!(h.tool_defs().len(), 2, "the new server is advertised once it is listed");
            h.list_missing().await;
            assert_eq!(h.tool_defs().len(), 4);
            assert_eq!(h.mcp_info("two").unwrap().tools, ["mcp__two_echo", "mcp__two_other"]);

            let r = h.mcp_sync(load(&[("srv", "--x"), ("two", "b")])).await;
            assert_eq!(r, McpSync { changed: vec!["srv".into()], ..Default::default() });
            assert!(h.slot(0).proc.borrow().is_none(), "stopped, to restart on the new config");
            assert_eq!(h.tool_defs().len(), 2, "the changed server is unlisted until listed again");
            h.list_missing().await;
            assert_eq!(h.tool_defs().len(), 4);

            let r = h.mcp_sync(load(&[("two", "b")])).await;
            assert_eq!(r, McpSync { removed: vec!["srv".into()], ..Default::default() });
            assert_eq!(h.mcp_names(), ["two"]);
            assert!(h.mcp_info("srv").is_none() && !h.has_tool("mcp__srv_echo"));

            let before = inits();
            assert_eq!(h.mcp_reconnect("two").await, Ok((2, false)));
            assert_eq!(inits(), before + 1, "reconnect starts the server again");
            assert!(h.mcp_reconnect("nope").await.is_err());
            h.shutdown_all().await;
        });
    }

    #[test]
    fn a_sibling_binary_is_found_when_the_running_exe_was_replaced() {
        let dir = std::env::temp_dir().join(format!("mu-sib-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mu-mcp-http"), "").unwrap();
        let exe = dir.join("mu (deleted)");
        assert!(std::fs::canonicalize(&exe).is_err());
        assert_eq!(sibling_of(&exe, "mu-mcp-http"), Some(dir.join("mu-mcp-http")));
        assert_eq!(sibling_of(&exe, "nope"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_terminates_a_stuck_plugin_group_before_killing_it() {
        run(async {
            let dir = std::env::temp_dir().join(format!("mu-plug-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let marker = dir.join("term");
            let script = format!(
                r#"trap 'echo term > "{}"' TERM
read -r line
id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
while :; do sleep 100 & wait; done
"#,
                marker.display()
            );
            std::fs::write(dir.join("p.sh"), script).unwrap();
            let m = Manifest {
                name: "stuck".into(),
                command: vec!["sh".into(), "p.sh".into()],
                lifetime: manifest::Lifetime::parse("session").unwrap(),
                hooks: vec![],
                provider: None,
                tools: vec![manifest::ToolSpec::with_schema("slow", json!({}))],
                dir: dir.clone(),
            };
            let (ntx, _nrx) = mpsc::unbounded_channel();
            let h = PluginHost::new(vec![m], vec![], ntx, Default::default());
            let proc = h.ensure(&h.slot(0)).await.unwrap();
            let pid = proc.pid as i32;
            proc.shutdown().await;
            assert!(marker.exists(), "the group got SIGTERM before SIGKILL");
            tokio::time::sleep(Duration::from_millis(200)).await;
            // SAFETY: signal 0 only checks that the group exists.
            assert_eq!(unsafe { libc::kill(-pid, 0) }, -1, "the process group is gone");
            let _ = std::fs::remove_dir_all(dir);
        });
    }
}
