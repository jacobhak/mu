//! The MCP lifecycle on the plugin host: the server slots, the handshake, listing and the cache,
//! login and reconnect, and the requests a stdio or http server may make of the client.

use super::host::{Kind, McpInfo, McpSync, PluginHost, Shared, Slot, stop_slot};
use super::process::{Proc, sibling_or_path};
use crate::mcp;
use mu_common::rpc::Request;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

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
pub(super) async fn mcp_request(
    proc: &Proc,
    m: &mcp::McpSlot,
    shared: &Shared,
    req: &Request,
) -> Result<Value, String> {
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

/// `tools/list` with `nextCursor` paging.
async fn list_tools(proc: &Proc, timeout: Option<Duration>) -> Result<Vec<mcp::cache::RemoteTool>, String> {
    if !proc.tools_cap() {
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
pub(super) async fn refresh_listing(slot: Rc<Slot>, proc: Rc<Proc>, shared: Rc<Shared>) {
    let Some(m) = slot.as_mcp() else { return };
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
    let instructions = if server.instructions { proc.instructions() } else { None };
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
    /// A slot for an MCP server, advertising its tools from the cache when there is one.
    pub(super) fn mcp_slot(
        s: mcp::config::McpServer,
        cwd: &str,
        cache_dir: &std::path::Path,
        seen: &mut std::collections::HashSet<String>,
        warnings: &mut Vec<String>,
        reserved: fn(&str) -> bool,
    ) -> Rc<Slot> {
        let ms = mcp::McpSlot::new(s, cwd);
        warnings.extend(ms.load_cache(cache_dir, seen, reserved));
        Rc::new(Slot {
            skip_hooks_until: Cell::new(None),
            kind: Kind::Mcp(Box::new(ms)),
            proc: RefCell::new(None),
            start: tokio::sync::Mutex::new(()),
        })
    }

    /// Names of the configured MCP servers.
    pub fn mcp_names(&self) -> Vec<String> {
        self.slots.borrow().iter().filter_map(|s| s.as_mcp().map(|m| m.name())).collect()
    }

    /// The runtime state of a configured MCP server.
    pub fn mcp_info(&self, name: &str) -> Option<McpInfo> {
        let slots = self.slots.borrow();
        let s = slots.iter().find(|s| s.as_mcp().is_some_and(|m| m.name() == name))?;
        let m = s.as_mcp()?;
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
                let Some(m) = s.as_mcp() else { return true };
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
                let found = slots.iter().find(|s| s.as_mcp().is_some_and(|m| m.name() == server.name)).cloned();
                let Some(slot) = found else {
                    out.added.push(server.name.clone());
                    let s =
                        Self::mcp_slot(server, &cwd, &self.shared.cache_dir, &mut seen, &mut warnings, self.reserved);
                    slots.push(s);
                    continue;
                };
                let m = slot.as_mcp().expect("an mcp slot");
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
                stop_slot(&s, &p).await;
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
                let m = s.as_mcp()?;
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
                    let m = s.as_mcp()?;
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
            .filter(|s| s.as_mcp().is_some_and(|m| !m.listed.get() && !m.failed.get()))
            .cloned()
            .collect();
        if todo.is_empty() {
            return;
        }
        let all = futures_util::future::join_all(todo.iter().map(|s| async move {
            if let Err(e) = self.list_slot(s).await {
                let m = s.as_mcp().expect("an mcp slot");
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
                        stop_slot(s, &p).await;
                    }
                }
            }
        }
    }

    /// The user-facing text for a server that needs an OAuth login. Only interactive mu can run the flow.
    pub(super) fn login_msg(&self, name: &str) -> String {
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

    pub fn is_interactive(&self) -> bool {
        self.shared.interactive.get()
    }

    /// Starts the server, lists its tools and makes them this slot's tools (and the cache's).
    async fn list_slot(&self, slot: &Rc<Slot>) -> Result<usize, String> {
        let m = slot.as_mcp().expect("an mcp slot");
        let server = m.server.borrow().clone();
        // Bounded even when the server's call timeout is 0 (no limit): connecting and listing are not calls.
        let timeout = Some(mcp::connect_timeout(&server));
        let run = async {
            let proc = self.ensure(slot).await?;
            match list_tools(&proc, timeout).await {
                Ok(t) => Ok((proc, t)),
                Err(e) => {
                    stop_slot(slot, &proc).await;
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
                    stop_slot(slot, &p).await;
                }
                return Err(format!("no answer within {}s", limit.as_secs()));
            }
        };
        let instructions = if server.instructions { proc.instructions() } else { None };
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
                && !p.used()
                && p.inflight.get() == 0
            {
                stop_slot(&slot, &p).await;
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
            .find(|s| s.as_mcp().is_some_and(|m| m.name() == name))
            .cloned()
            .ok_or_else(|| format!("no MCP server {name}"))?;
        let m = slot.as_mcp().expect("an mcp slot");
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
            stop_slot(&slot, &p).await;
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

    pub(super) async fn mcp_command(&self, m: &mcp::McpSlot) -> Result<Command, String> {
        use mcp::config::Kind;
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

    /// MCP `initialize` and `notifications/initialized`; records the capabilities and instructions.
    pub(super) async fn mcp_handshake(&self, proc: &Rc<Proc>, server: &mcp::config::McpServer) -> Result<(), String> {
        let params = json!({
            "protocolVersion": mcp::PROTOCOL_VERSION,
            "capabilities": {"roots": {}},
            "clientInfo": {"name": "mu", "version": env!("CARGO_PKG_VERSION")},
        });
        let r = proc.request("initialize", params, Some(mcp::connect_timeout(server))).await?;
        let w = proc.mcp().expect("an mcp process");
        w.tools_cap.set(r["capabilities"].get("tools").is_some_and(|t| !t.is_null()));
        *w.instructions.borrow_mut() = r["instructions"].as_str().filter(|s| !s.trim().is_empty()).map(String::from);
        let n = Request::new(None, "notifications/initialized", json!({}));
        proc.write_line(&serde_json::to_string(&n).expect("notification serializes")).await
    }
}
