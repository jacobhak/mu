pub mod manifest;

use crate::provider::{Providers, ToolDef};
use manifest::{Lifetime, Manifest};
use mu_common::rpc::{Frame, Request, Response, parse_frame};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::process::Stdio;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub display: Option<String>,
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
}

struct Proc {
    name: String,
    pid: u32,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: RefCell<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next: Cell<u64>,
    inflight: Cell<u32>,
    last: Cell<Instant>,
    dead: watch::Sender<bool>,
}

struct Slot {
    manifest: Manifest,
    proc: RefCell<Option<Rc<Proc>>>,
    start: tokio::sync::Mutex<()>,
}

pub struct PluginHost {
    slots: Vec<Rc<Slot>>,
    shared: Rc<Shared>,
    pub warnings: Vec<String>,
}

fn resolve_command(m: &Manifest) -> std::path::PathBuf {
    let c = &m.command[0];
    if c.contains('/') {
        return m.dir.join(c);
    }
    if let Some(sib) = std::env::current_exe().and_then(std::fs::canonicalize).ok().and_then(|e| e.parent().map(|d| d.join(c))).filter(|p| p.exists()) {
        return sib;
    }
    c.into()
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

    async fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> Result<Value, String> {
        let id = self.next.get() + 1;
        self.next.set(id);
        let (tx, rx) = oneshot::channel();
        self.pending.borrow_mut().insert(id, tx);
        self.inflight.set(self.inflight.get() + 1);
        self.last.set(Instant::now());
        let line = serde_json::to_string(&Request::new(Some(json!(id)), method, params)).unwrap();
        let res = async {
            self.write_line(&line).await?;
            match timeout {
                Some(t) => tokio::time::timeout(t, rx).await.map_err(|_| format!("plugin {} timed out on {method}", self.name))?,
                None => rx.await,
            }
            .unwrap_or_else(|_| Err(format!("plugin {} exited", self.name)))
        }
        .await;
        self.pending.borrow_mut().remove(&id);
        self.inflight.set(self.inflight.get() - 1);
        self.last.set(Instant::now());
        res
    }

    async fn shutdown(&self) {
        let _ = self.write_line(&serde_json::to_string(&Request::new(None, "shutdown", json!({}))).unwrap()).await;
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
            unsafe { libc::kill(-(self.pid as i32), libc::SIGKILL) };
        }
    }
}

async fn read_loop(proc: Rc<Proc>, slot: Rc<Slot>, shared: Rc<Shared>, stdout: tokio::process::ChildStdout, mut child: tokio::process::Child) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match parse_frame(&line) {
            Some(Frame::Response(r)) => {
                let Some(id) = r.id.as_u64() else { continue };
                if let Some(tx) = proc.pending.borrow_mut().remove(&id) {
                    let _ = tx.send(match r.error {
                        Some(e) => Err(e.message),
                        None => Ok(r.result.unwrap_or(Value::Null)),
                    });
                }
            }
            Some(Frame::Request(req)) => {
                let (p, sh) = (proc.clone(), shared.clone());
                tokio::task::spawn_local(async move { handle_plugin_request(p, sh, req).await });
            }
            None => {}
        }
    }
    let _ = child.wait().await;
    proc.stdin.lock().await.take();
    for (_, tx) in proc.pending.borrow_mut().drain() {
        let _ = tx.send(Err(format!("plugin {} exited", proc.name)));
    }
    let mut cur = slot.proc.borrow_mut();
    if cur.as_ref().is_some_and(|c| Rc::ptr_eq(c, &proc)) {
        *cur = None;
    }
    let _ = proc.dead.send(true);
}

async fn handle_plugin_request(proc: Rc<Proc>, shared: Rc<Shared>, req: Request) {
    let p = &req.params;
    let result: Result<Value, String> = match req.method.as_str() {
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
            Err(e) => Response::err(id, -32601, e),
        };
        let _ = proc.write_line(&serde_json::to_string(&resp).unwrap()).await;
    }
}

impl PluginHost {
    pub fn new(manifests: Vec<Manifest>, warnings: Vec<String>, notices: mpsc::UnboundedSender<String>, init: InitParams) -> Rc<Self> {
        let mut warnings = warnings;
        let mut seen = std::collections::HashSet::new();
        let slots = manifests
            .into_iter()
            .map(|mut m| {
                m.tools.retain(|t| {
                    let clash = crate::tools::BUILTIN_NAMES.contains(&t.name.as_str()) || !seen.insert(t.name.clone());
                    if clash {
                        warnings.push(format!("plugin {}: tool `{}` skipped (name collision)", m.name, t.name));
                    }
                    !clash
                });
                Rc::new(Slot { manifest: m, proc: RefCell::new(None), start: tokio::sync::Mutex::new(()) })
            })
            .collect();
        let shared = Rc::new(Shared { providers: RefCell::new(Weak::new()), notices, init: RefCell::new(init) });
        Rc::new(Self { slots, shared, warnings })
    }

    pub fn set_providers(&self, p: &Rc<Providers>) {
        *self.shared.providers.borrow_mut() = Rc::downgrade(p);
    }

    pub fn set_session(&self, init: InitParams) {
        *self.shared.init.borrow_mut() = init;
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.slots
            .iter()
            .flat_map(|s| s.manifest.tools.iter())
            .map(|t| ToolDef { name: t.name.clone(), description: t.description.clone(), schema: t.schema.clone() })
            .collect()
    }

    fn slot_for_tool(&self, name: &str) -> Option<&Rc<Slot>> {
        self.slots.iter().find(|s| s.manifest.tools.iter().any(|t| t.name == name))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.slot_for_tool(name).is_some()
    }

    pub fn status(&self) -> Vec<(String, String, bool)> {
        self.slots.iter().map(|s| (s.manifest.name.clone(), s.manifest.lifetime.label(), s.proc.borrow().is_some())).collect()
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
        let mut cmd = Command::new(resolve_command(m));
        cmd.args(&m.command[1..]).current_dir(&m.dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(log);
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
            pending: RefCell::new(HashMap::new()),
            next: Cell::new(0),
            inflight: Cell::new(0),
            last: Cell::new(Instant::now()),
            dead,
        });
        *slot.proc.borrow_mut() = Some(proc.clone());
        tokio::task::spawn_local(read_loop(proc.clone(), slot.clone(), self.shared.clone(), stdout, child));
        let init = self.shared.init.borrow().clone();
        let params = json!({
            "protocolVersion": 1,
            "cwd": init.cwd,
            "sessionId": init.session_id,
            "sessionFile": init.session_file,
            "muHome": mu_common::paths::mu_home(),
            "agentDir": mu_common::paths::omp_agent_dir(),
        });
        if let Err(e) = proc.request("initialize", params, Some(Duration::from_secs(20))).await {
            proc.shutdown().await;
            return Err(format!("plugin {} failed to start: {e}", m.name));
        }
        if let Lifetime::Idle(d) = m.lifetime {
            let p = Rc::downgrade(&proc);
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
                        pr.shutdown().await;
                        return;
                    }
                }
            });
        }
        Ok(proc)
    }

    pub async fn call_tool(&self, name: &str, args: &Value, cancel: &CancellationToken) -> ToolResult {
        let fail = |e: String| ToolResult { content: e, is_error: true, display: None };
        let Some(slot) = self.slot_for_tool(name) else { return fail(format!("Unknown tool: {name}")) };
        let proc = match self.ensure(slot).await {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        let res = tokio::select! {
            r = proc.request("tool/call", json!({"name": name, "arguments": args}), None) => r,
            _ = cancel.cancelled() => Err("Tool call aborted by user".to_string()),
        };
        if slot.manifest.lifetime == Lifetime::Call {
            proc.shutdown().await;
        }
        match res {
            Ok(v) => ToolResult {
                content: v["content"].as_str().unwrap_or_default().into(),
                is_error: v["isError"].as_bool().unwrap_or(false),
                display: v["display"].as_str().map(String::from),
            },
            Err(e) => fail(e),
        }
    }

    /// One request to a named plugin, starting it if needed.
    pub async fn request(&self, plugin: &str, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let slot = self.slots.iter().find(|s| s.manifest.name == plugin).ok_or_else(|| format!("no plugin {plugin}"))?;
        let proc = self.ensure(slot).await?;
        proc.request(method, params, Some(timeout)).await
    }

    /// Runs `hook/<event>` on every plugin subscribed to `sub` (`event` or `event:key`), in name order.
    /// Failures and timeouts become notices and are skipped.
    pub async fn hook(&self, sub: &str, params: Value) -> Vec<(String, Value)> {
        let event = sub.split(':').next().unwrap_or(sub);
        let timeout = Duration::from_secs(if event == "session_end" { 10 } else { 30 });
        let mut out = Vec::new();
        for slot in self.slots.iter().filter(|s| s.manifest.hooks.iter().any(|h| h == sub)) {
            let name = &slot.manifest.name;
            let r = match self.ensure(slot).await {
                Ok(p) => p.request(&format!("hook/{event}"), params.clone(), Some(timeout)).await,
                Err(e) => Err(e),
            };
            match r {
                Ok(v) => out.push((name.clone(), v)),
                Err(e) => {
                    let _ = self.shared.notices.send(format!("plugin {name}: hook {event} failed: {e}"));
                }
            }
        }
        out
    }

    pub async fn provider_endpoint(&self, kind: &str) -> Result<(String, String), String> {
        let slot = self
            .slots
            .iter()
            .find(|s| s.manifest.provider.as_deref() == Some(kind))
            .ok_or_else(|| "no gateway: install the gateway plugin (~/.mu/plugins/gateway) or set MU_GATEWAY_URL".to_string())?;
        let proc = self.ensure(slot).await?;
        let v = proc.request("provider/endpoint", json!({"clientPid": std::process::id()}), Some(Duration::from_secs(45))).await?;
        match (v["baseUrl"].as_str(), v["token"].as_str()) {
            (Some(b), Some(t)) => Ok((b.trim_end_matches('/').into(), t.into())),
            _ => Err("gateway plugin returned no endpoint".into()),
        }
    }

    pub async fn shutdown_all(&self) {
        for s in &self.slots {
            let p = s.proc.borrow().clone();
            if let Some(p) = p {
                p.shutdown().await;
            }
        }
    }
}
