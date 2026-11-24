//! MCP servers and plugin status (table row "MCP"): the configured servers and their state, changes to
//! the config files, trust, and bringing the running servers in line.

use super::local::{Local, Registry};
use super::{Answer, pending, ready};
use mu_common::paths;
use mu_tools::mcp::config::{self, Entry, Kind};
use mu_tools::mcp::discover::{self, Found, Roots};
use mu_tools::mcp::store::{self, Level};
use mu_tools::mcp::trust;
use mu_tools::plugin::host::McpInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// `with_found` -> `McpServers`.
pub const MCP_SERVERS: &str = "mcp_servers";
/// `show_warnings` -> `Applied` (pending).
pub const MCP_APPLY: &str = "mcp_apply";
/// `(name, verbose)` -> `Reconnected` (pending).
pub const MCP_RECONNECT: &str = "mcp_reconnect";
/// `files`, or `None` for every file waiting for trust -> how many were trusted.
pub const MCP_TRUST: &str = "mcp_trust";
pub const MCP_UNTRUST: &str = "mcp_untrust";
/// `name` -> tool count and whether the tool set changed (pending).
pub const MCP_AFTER_LOGIN: &str = "mcp_after_login";
/// The names of the loaded servers.
pub const MCP_NAMES: &str = "mcp_names";
/// Names of environment variables -> those that are set and non-empty.
pub const MCP_ENV_SET: &str = "mcp_env_set";
/// `(level, name, entry)` -> the file written.
pub const MCP_ADD: &str = "mcp_add";
/// `name` -> the files it was removed from.
pub const MCP_REMOVE: &str = "mcp_remove";
/// `(name, on)` -> whether anything changed.
pub const MCP_SET_ENABLED: &str = "mcp_set_enabled";

/// One configured server, as the lists show it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerRow {
    pub name: String,
    pub kind_label: String,
    /// The command, or `scheme://host` of the url.
    pub target: String,
    /// `user`, `project` or the file.
    pub level: String,
    pub state: String,
    pub enabled: bool,
    pub needs_login: bool,
    pub wants_oauth: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct McpServers {
    pub servers: Vec<ServerRow>,
    pub warnings: Vec<String>,
    /// Servers other tools have configured; empty unless asked for.
    pub found: Vec<Found>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Applied {
    pub lines: Vec<String>,
    /// The tool list changed.
    pub refresh: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reconnected {
    pub tools: usize,
    pub changed: bool,
    pub ms: u64,
    /// The tool names under the status line, when `verbose`.
    pub listing: String,
}

pub(super) fn register(r: &mut Registry) {
    r.add(MCP_SERVERS, servers);
    r.add(MCP_APPLY, apply);
    r.add(MCP_RECONNECT, reconnect);
    r.add(MCP_TRUST, trust_files);
    r.add(MCP_UNTRUST, untrust);
    r.add(MCP_AFTER_LOGIN, after_login);
    r.add(MCP_NAMES, names);
    r.add(MCP_ENV_SET, env_set);
    r.add(MCP_ADD, add);
    r.add(MCP_REMOVE, remove);
    r.add(MCP_SET_ENABLED, set_enabled);
}

/// `scheme://host` of a url: paths, queries and credentials may carry secrets.
fn origin(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return String::new() };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    format!("{scheme}://{}", authority.rsplit('@').next().unwrap_or(authority))
}

fn describe(info: &McpInfo, name: &str) -> String {
    if info.needs_login {
        format!("needs login: /mcp login {name}")
    } else if info.failed {
        "failed to list its tools".into()
    } else if info.listed {
        format!("{} tools{}", info.tools.len(), if info.running { ", running" } else { "" })
    } else {
        "not connected yet; its tools are listed on first use".into()
    }
}

/// The tool names under a server's status line, as many as fit.
fn tool_names(info: &McpInfo) -> String {
    const SHOWN: usize = 8;
    let mut s: String = info.tools.iter().take(SHOWN).map(|t| format!("\n  {t}")).collect();
    if info.tools.len() > SHOWN {
        s.push_str(&format!("\n  … and {} more", info.tools.len() - SHOWN));
    }
    s
}

fn level_of(l: &Local, e: &Entry, user_file: &std::path::Path) -> String {
    if e.file == user_file {
        "user".to_string()
    } else if e.file.starts_with(l.cwd()) {
        "project".to_string()
    } else {
        e.file.display().to_string()
    }
}

fn state_of(l: &Local, e: &Entry) -> String {
    if !e.trusted {
        return "(untrusted)".into();
    }
    if !e.enabled {
        return "disabled".into();
    }
    match l.plugins.mcp_info(&e.server.name) {
        Some(i) => describe(&i, &e.server.name),
        None => "enabled, not loaded yet: /mcp reload".into(),
    }
}

fn servers(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |with_found: bool| {
        let agent = paths::omp_agent_dir();
        let cwd = l.cwd();
        let (all, warnings) = config::load_all(&cwd, &agent);
        let user_file = agent.join("mcp.json");
        let found = if with_found {
            let roots = Roots { home: paths::home(), cwd, agent_dir: agent };
            discover::scan(&roots, &all)
        } else {
            Vec::new()
        };
        let servers = all
            .iter()
            .map(|e| {
                let s = &e.server;
                ServerRow {
                    name: s.name.clone(),
                    kind_label: s.kind_label().to_string(),
                    target: match &s.kind {
                        Kind::Http { url, .. } => origin(url),
                        Kind::Stdio { command, .. } => command.clone(),
                    },
                    level: level_of(l, e, &user_file),
                    state: state_of(l, e),
                    enabled: e.enabled,
                    needs_login: l.plugins.mcp_info(&s.name).is_some_and(|i| i.needs_login),
                    wants_oauth: s.wants_oauth(),
                }
            })
            .collect();
        Ok(McpServers { servers, warnings, found })
    })
}

/// Re-reads the config and brings the running servers in line with it. New and changed servers are
/// connected so their tools are known right away.
fn apply(l: &Rc<Local>, args: Value) -> Answer {
    pending(args, |show_warnings: bool| {
        let (plugins, cwd) = (l.plugins.clone(), l.cwd());
        async move {
            let (servers, warnings) = config::load(&cwd, &paths::omp_agent_dir());
            let sync = plugins.mcp_sync(servers).await;
            if !sync.added.is_empty() || !sync.changed.is_empty() {
                plugins.list_missing_until(&CancellationToken::new()).await;
            }
            let mut lines: Vec<String> = Vec::new();
            if show_warnings {
                lines.extend(warnings.into_iter().map(|w| format!("warning: {w}")));
            }
            lines.extend(sync.removed.iter().map(|n| format!("mcp:{n}: stopped")));
            for (verb, names) in [("added", &sync.added), ("restarted on its new config", &sync.changed)] {
                for n in names {
                    lines.push(match plugins.mcp_info(n) {
                        Some(i) => format!("mcp:{n}: {verb}; {}{}", describe(&i, n), tool_names(&i)),
                        None => format!("mcp:{n}: {verb}; gone"),
                    });
                }
            }
            if lines.is_empty() {
                lines.push("no change to the running servers".into());
            }
            Ok(Applied { lines, refresh: !sync.is_empty() })
        }
    })
}

fn reconnect(l: &Rc<Local>, args: Value) -> Answer {
    pending(args, |(name, verbose): (String, bool)| {
        let plugins = l.plugins.clone();
        async move {
            let t = Instant::now();
            let (tools, changed) = plugins.mcp_reconnect(&name).await?;
            let ms = t.elapsed().as_millis() as u64;
            let listing = if verbose {
                plugins.mcp_info(&name).map(|i| tool_names(&i)).unwrap_or_default()
            } else {
                String::new()
            };
            Ok(Reconnected { tools, changed, ms, listing })
        }
    })
}

/// Trusts each file's server set as the offer showed it; `None` trusts what is waiting.
fn trust_files(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |files: Option<Vec<(PathBuf, String)>>| {
        let files = match files {
            Some(f) => f,
            None => config::untrusted_files(&l.cwd(), &paths::omp_agent_dir())
                .into_iter()
                .map(|(file, digest, _)| (file, digest))
                .collect(),
        };
        for (file, digest) in &files {
            trust::trust(file, digest).map_err(|e| format!("{}: {e}", file.display()))?;
        }
        Ok(files.len())
    })
}

fn untrust(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |(): ()| {
        let cwd = l.cwd();
        for file in config::candidate_files(&cwd, &paths::omp_agent_dir()) {
            if trust::is_project_file(&cwd, &file) {
                trust::untrust(&file);
            }
        }
        Ok(())
    })
}

fn after_login(l: &Rc<Local>, args: Value) -> Answer {
    pending(args, |name: String| {
        let plugins = l.plugins.clone();
        async move { plugins.after_login(&name).await }
    })
}

fn names(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |(): ()| Ok(l.plugins.mcp_names()))
}

/// Whether a variable is set and non-empty. Unlike the key flow's check, blanks and non-UTF-8 count.
fn env_set(_: &Rc<Local>, args: Value) -> Answer {
    ready(args, |names: Vec<String>| {
        Ok(names.into_iter().filter(|n| std::env::var_os(n).is_some_and(|v| !v.is_empty())).collect::<Vec<_>>())
    })
}

fn add(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |(level, name, entry): (Level, String, Value)| {
        store::add(level, &l.cwd(), &paths::omp_agent_dir(), &name, entry)
    })
}

fn remove(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |name: String| store::remove(&l.cwd(), &paths::omp_agent_dir(), &name))
}

fn set_enabled(l: &Rc<Local>, args: Value) -> Answer {
    ready(args, |(name, on): (String, bool)| store::set_enabled(&l.cwd(), &paths::omp_agent_dir(), &name, on))
}
