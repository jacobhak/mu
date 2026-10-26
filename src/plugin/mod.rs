pub mod manifest;

use crate::provider::{Providers, ToolDef};
use manifest::{Lifetime, Manifest};
use mu_common::rpc::{Frame, Framed, Line, Pending, Request, Response};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::process::Stdio;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, watch};
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
    pending: Pending,
    inflight: Cell<u32>,
    last: Cell<Instant>,
    dead: watch::Sender<bool>,
}

struct Slot {
    /// Hooks for this plugin are skipped until then (it timed out on one).
    skip_hooks_until: Cell<Option<Instant>>,
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
    if let Some(sib) = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(c)))
        .filter(|p| p.exists())
    {
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

    async fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> Result<Value, String> {
        let (id, rx) = self.pending.register();
        self.inflight.set(self.inflight.get() + 1);
        self.last.set(Instant::now());
        // Runs on every exit, including this future being dropped (a cancelled call).
        let _guard = InflightGuard { proc: self, id };
        let line = serde_json::to_string(&Request::new(Some(json!(id)), method, params)).unwrap();
        async {
            self.write_line(&line).await?;
            match timeout {
                Some(t) => tokio::time::timeout(t, rx)
                    .await
                    .map_err(|_| format!("plugin {} timed out on {method}", self.name))?,
                None => rx.await,
            }
            .unwrap_or_else(|_| Err(format!("plugin {} exited", self.name)))
        }
        .await
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
                let Some(id) = r.id.as_u64() else { continue };
                proc.pending.resolve(
                    id,
                    match r.error {
                        Some(e) => Err(e.message),
                        None => Ok(r.result.unwrap_or(Value::Null)),
                    },
                );
            }
            Some(Frame::Request(req)) => {
                let (p, sh) = (proc.clone(), shared.clone());
                tokio::task::spawn_local(async move { handle_plugin_request(p, sh, req).await });
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
    pub fn new(
        manifests: Vec<Manifest>,
        warnings: Vec<String>,
        notices: mpsc::UnboundedSender<String>,
        init: InitParams,
    ) -> Rc<Self> {
        let mut warnings = warnings;
        let mut seen = std::collections::HashSet::new();
        let slots = manifests
            .into_iter()
            .map(|mut m| {
                m.tools.retain(|t| {
                    let clash = crate::tools::is_builtin(&t.name) || !seen.insert(t.name.clone());
                    if clash {
                        warnings.push(format!("plugin {}: tool `{}` skipped (name collision)", m.name, t.name));
                    }
                    !clash
                });
                Rc::new(Slot {
                    skip_hooks_until: Cell::new(None),
                    manifest: m,
                    proc: RefCell::new(None),
                    start: tokio::sync::Mutex::new(()),
                })
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

    fn slot_for_tool(&self, name: &str) -> Option<&Rc<Slot>> {
        self.slots.iter().find(|s| s.manifest.tools.iter().any(|t| t.name == name))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.slot_for_tool(name).is_some()
    }

    pub fn status(&self) -> Vec<(String, String, bool)> {
        self.slots
            .iter()
            .map(|s| (s.manifest.name.clone(), s.manifest.lifetime.label(), s.proc.borrow().is_some()))
            .collect()
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
            pending: Pending::default(),
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
            slot.detach(&proc);
            proc.shutdown().await;
            return Err(format!("plugin {} failed to start: {e}", m.name));
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
            slot.detach(&proc);
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
        let slot =
            self.slots.iter().find(|s| s.manifest.name == plugin).ok_or_else(|| format!("no plugin {plugin}"))?;
        let proc = self.ensure(slot).await?;
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
        let slots: Vec<&Rc<Slot>> = self
            .slots
            .iter()
            .filter(|s| s.manifest.hooks.iter().any(|h| h == sub))
            .filter(|s| s.skip_hooks_until.get().is_none_or(|t| now >= t))
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
        let slot = self.slots.iter().find(|s| s.manifest.provider.as_deref() == Some(kind)).ok_or_else(|| {
            "no gateway: install the gateway plugin (~/.mu/plugins/gateway) or set MU_GATEWAY_URL".to_string()
        })?;
        let proc = self.ensure(slot).await?;
        let v = proc
            .request("provider/endpoint", json!({"clientPid": std::process::id()}), Some(Duration::from_secs(45)))
            .await?;
        match (v["baseUrl"].as_str(), v["token"].as_str()) {
            (Some(b), Some(t)) => Ok((b.trim_end_matches('/').into(), t.into())),
            _ => Err("gateway plugin returned no endpoint".into()),
        }
    }

    pub async fn shutdown_all(&self) {
        for s in &self.slots {
            let p = s.proc.borrow().clone();
            if let Some(p) = p {
                s.detach(&p);
                p.shutdown().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn run<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(tokio::task::LocalSet::new().run_until(f))
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
            assert!(r.is_error && r.content.contains("aborted"), "{}", r.content);
            let proc = h.slots[0].proc.borrow().clone().unwrap();
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
            assert!(r.is_error, "{}", r.content);
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(h.slots[0].proc.borrow().is_none(), "slot cleared so the next call restarts it");
            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn a_call_during_idle_shutdown_gets_a_fresh_process() {
        run(async {
            let (h, dir) =
                host("printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"content\":\"ok\"}}\\n' \"$id\"", "idle:1s");
            let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
            assert_eq!(r.content, "ok");
            let first = h.slots[0].proc.borrow().clone().unwrap().pid;
            tokio::time::sleep(Duration::from_millis(1600)).await;
            let r = h.call_tool("slow", &json!({}), &CancellationToken::new()).await;
            assert_eq!(r.content, "ok", "{}", r.content);
            let second = h.slots[0].proc.borrow().clone().unwrap().pid;
            assert_ne!(first, second);
            h.shutdown_all().await;
            let _ = std::fs::remove_dir_all(dir);
        });
    }
}
