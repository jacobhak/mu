//! The plugin host: the registry of native plugins and MCP servers, tool listing and dispatch, hooks.

use super::manifest::{self, Lifetime, Manifest};
use super::process::Proc;
use crate::mcp;
use mu_base::tool::{ToolDef, ToolOutput};
use mu_provider::provider::client::Providers;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct InitParams {
    pub cwd: String,
    pub session_id: String,
    pub session_file: Option<String>,
}

/// State shared with every plugin process's reader task.
pub(super) struct Shared {
    pub(super) providers: RefCell<Weak<Providers>>,
    pub(super) notices: mpsc::UnboundedSender<String>,
    pub(super) init: RefCell<InitParams>,
    /// The plugin that serves `provider/endpoint`, asked for the auth broker's address.
    pub(super) gateway: RefCell<Option<Weak<Slot>>>,
    pub(super) cache_dir: std::path::PathBuf,
    /// A terminal UI is attached, so `/login mcp:<name>` can be offered.
    pub(super) interactive: Cell<bool>,
}

/// What a slot runs: a native plugin from its manifest, or an MCP server with its own state.
pub(super) enum Kind {
    Native { manifest: Manifest },
    Mcp(Box<mcp::McpSlot>),
}

pub(super) struct Slot {
    /// Hooks for this plugin are skipped until then (it timed out on one).
    pub(super) skip_hooks_until: Cell<Option<Instant>>,
    pub(super) kind: Kind,
    pub(super) proc: RefCell<Option<Rc<Proc>>>,
    pub(super) start: tokio::sync::Mutex<()>,
}

impl Slot {
    /// `<plugin>`, or `mcp:<server>` for an MCP server.
    pub(super) fn name(&self) -> String {
        match &self.kind {
            Kind::Native { manifest } => manifest.name.clone(),
            Kind::Mcp(m) => format!("mcp:{}", m.name()),
        }
    }

    /// What is started: the manifest's first command word, or the MCP server's name.
    pub(super) fn program(&self) -> String {
        match &self.kind {
            Kind::Native { manifest } => manifest.command[0].clone(),
            Kind::Mcp(m) => m.name(),
        }
    }

    pub(super) fn native(&self) -> Option<&Manifest> {
        match &self.kind {
            Kind::Native { manifest } => Some(manifest),
            Kind::Mcp(_) => None,
        }
    }

    pub(super) fn as_mcp(&self) -> Option<&mcp::McpSlot> {
        match &self.kind {
            Kind::Mcp(m) => Some(m),
            Kind::Native { .. } => None,
        }
    }

    /// Hook subscriptions; an MCP server has none.
    pub(super) fn hooks(&self) -> &[String] {
        match &self.kind {
            Kind::Native { manifest } => &manifest.hooks,
            Kind::Mcp(_) => &[],
        }
    }

    /// Forgets `proc` as this slot's process, so the next call starts a fresh one.
    pub(super) fn detach(&self, proc: &Rc<Proc>) {
        let mut cur = self.proc.borrow_mut();
        if cur.as_ref().is_some_and(|c| Rc::ptr_eq(c, proc)) {
            *cur = None;
        }
    }
}

/// Forgets `proc` as `slot`'s process and shuts it down. Detaching first means a call that arrives
/// during the shutdown starts a fresh process.
pub(super) async fn stop_slot(slot: &Slot, proc: &Rc<Proc>) {
    slot.detach(proc);
    proc.shutdown().await;
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
    pub(super) slots: RefCell<Vec<Rc<Slot>>>,
    pub(super) shared: Rc<Shared>,
    pub warnings: Vec<String>,
    /// Every advertised MCP tool name, so a later listing can tell a clash.
    pub(super) seen: RefCell<std::collections::HashSet<String>>,
    /// The lifetime of an MCP server added after startup.
    pub(super) mcp_lifetime: Lifetime,
    /// Built-in tool names: a plugin or MCP tool may not take one.
    pub(super) reserved: fn(&str) -> bool,
}

impl PluginHost {
    #[cfg(any(test, feature = "test-support"))]
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
                    kind: Kind::Native { manifest: m },
                    proc: RefCell::new(None),
                    start: tokio::sync::Mutex::new(()),
                })
            })
            .collect();
        for s in servers {
            slots.push(Self::mcp_slot(s, &init.cwd, &cache_dir, &mut seen, &mut warnings, reserved));
        }
        let gateway = slots.iter().find(|s| s.native().is_some_and(|m| m.provider.is_some())).map(Rc::downgrade);
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

    pub fn set_providers(&self, p: &Rc<Providers>) {
        *self.shared.providers.borrow_mut() = Rc::downgrade(p);
    }

    pub fn set_session(&self, init: InitParams) {
        *self.shared.init.borrow_mut() = init;
    }

    /// The lifetime a slot's process follows: the manifest's, or the configured one for MCP servers.
    pub(super) fn lifetime_of(&self, slot: &Slot) -> Lifetime {
        match &slot.kind {
            Kind::Native { manifest } => manifest.lifetime,
            Kind::Mcp(_) => self.mcp_lifetime,
        }
    }

    /// Every tool a slot offers: its manifest's, or an MCP server's listed ones.
    fn specs(slot: &Slot) -> Vec<manifest::ToolSpec> {
        match &slot.kind {
            Kind::Mcp(m) => m.tools.borrow().iter().map(|t| t.spec.clone()).collect(),
            Kind::Native { manifest } => manifest.tools.clone(),
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
            .find(|s| match &s.kind {
                Kind::Mcp(m) => m.tools.borrow().iter().any(|t| t.spec.name == name),
                Kind::Native { manifest } => manifest.tools.iter().any(|t| t.name == name),
            })
            .cloned()
    }

    pub(crate) fn has_tool(&self, name: &str) -> bool {
        self.slot_for_tool(name).is_some()
    }

    /// The tool belongs to an MCP server (as opposed to a native plugin).
    pub(crate) fn is_mcp_tool(&self, name: &str) -> bool {
        self.slot_for_tool(name).is_some_and(|s| s.as_mcp().is_some())
    }

    /// `(name, description, running, extra)` per plugin; an MCP server is `mcp:<name>` with its kind
    /// and lifetime, and its tool count as `extra`.
    pub fn status(&self) -> Vec<(String, String, bool, String)> {
        self.slots
            .borrow()
            .iter()
            .map(|s| {
                let (label, extra) = match s.as_mcp() {
                    Some(m) => {
                        let login = if m.needs_login.get() { " · needs login" } else { "" };
                        let label =
                            format!("{} · {}{login}", m.server.borrow().kind_label(), self.lifetime_of(s).label());
                        (label, format!("{} tools", m.tools.borrow().len()))
                    }
                    None => (self.lifetime_of(s).label(), String::new()),
                };
                (s.name(), label, s.proc.borrow().is_some(), extra)
            })
            .collect()
    }

    pub(crate) async fn call_tool(&self, name: &str, args: &Value, cancel: &CancellationToken) -> ToolOutput {
        let fail = |e: String| ToolOutput::err(e);
        let Some(slot) = self.slot_for_tool(name) else { return fail(format!("Unknown tool: {name}")) };
        let proc = match self.ensure(&slot).await {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        if let Some(w) = proc.mcp() {
            w.used.set(true);
        }
        let mcp_call = slot.as_mcp().and_then(|m| {
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
        if self.lifetime_of(&slot) == Lifetime::Call {
            stop_slot(&slot, &proc).await;
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
                    changes: Vec::new(),
                }
            }
            (Ok(v), None) => ToolOutput {
                text: v["content"].as_str().unwrap_or_default().into(),
                is_error: v["isError"].as_bool().unwrap_or(false),
                display: v["display"].as_str().map(String::from),
                details: None,
                meta: None,
                images: Vec::new(),
                changes: Vec::new(),
            },
            (Err(e), call) => {
                let msg = match slot.as_mcp() {
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
            .find(|s| s.name() == plugin)
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
            .filter(|s| s.hooks().iter().any(|h| h == sub))
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
            let name = slot.name();
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

    pub(crate) async fn provider_endpoint(&self, kind: &str) -> Result<(String, String), String> {
        let slot =
            self.slots.borrow().iter().find(|s| s.native().and_then(|m| m.provider.as_deref()) == Some(kind)).cloned();
        let Some(slot) = slot else {
            let dir = mu_common::paths::mu_home().join("plugins");
            let found: Vec<String> =
                self.slots.borrow().iter().filter(|s| s.native().is_some()).map(|s| s.name()).collect();
            let found = if found.is_empty() { "no plugins".to_string() } else { found.join(", ") };
            return Err(format!(
                "no gateway: no plugin in {} provides `{kind}` (loaded: {found}); install the gateway plugin there or set MU_GATEWAY_URL",
                dir.display()
            ));
        };
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
    pub fn gateway_start(self: &Rc<Self>) -> mu_provider::provider::endpoint::StartFn {
        let host = self.clone();
        mu_provider::provider::endpoint::start_fn(move || {
            let host = host.clone();
            async move {
                match mu_provider::gateway::env_endpoint() {
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
                stop_slot(s, &p).await;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn slot(&self, i: usize) -> Rc<Slot> {
        self.slots.borrow()[i].clone()
    }
}
