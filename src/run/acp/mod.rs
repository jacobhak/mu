//! `--acp`: Agent Client Protocol server over stdio (newline-delimited JSON-RPC).
//! One mu `Agent` per ACP session, so each session keeps its own working directory.

use crate::cli::select::pick_model;
use crate::run::setup::{Setup, setup};
use anyhow::{Result, anyhow};
use mu_agent::agent;
use mu_agent::handle::{AgentHandle, AgentSetup, Mode, build_agent};
use mu_base::events::{AgentEvent, Command};
use mu_base::message::{Image, UserInput};
use mu_base::models::Thinking;
use mu_provider::model::pricing;
use mu_session::session;
use mu_tools::plugin::host::PluginHost;
use mu_tools::tools::summary;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL: i64 = -32603;

pub fn run(model: Option<String>, thinking: Option<Thinking>) -> Result<i32> {
    mu_session::session::set_run_info("acp", None);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        mu_tools::plugin::process::exit_on_signals();
        serve(model, thinking).await
    });
    Ok(0)
}

fn send(v: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, v);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn respond(id: &Value, result: Value) {
    send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn fail(id: &Value, code: i64, msg: impl Into<String>) {
    send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg.into()}}));
}

fn update(sid: &str, u: Value) {
    send(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}}));
}

fn spawn_stdin() -> UnboundedReceiver<Value> {
    use mu_common::rpc::{Framed, Line};
    let (tx, rx) = unbounded_channel();
    tokio::task::spawn_local(async move {
        let mut input = Framed::new(tokio::io::BufReader::new(tokio::io::stdin()));
        while let Some(line) = input.next_line().await {
            match line {
                Line::Value(v) => {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
                Line::Bad(e) => fail(&Value::Null, PARSE_ERROR, format!("parse error: {e}")),
            }
        }
    });
    rx
}

/// The model id a `model` request waits for: an advertised id as is, or a role selector
/// (`@slow`, `@slow:high`) resolved the way `--model` is. Fails at once for anything else.
fn model_target(
    value: &str,
    models: &[String],
    resolve: impl Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    if models.iter().any(|m| m == value) {
        Ok(value.into())
    } else if value.starts_with('@') {
        resolve(value)
    } else {
        Err(format!("unknown model {value}"))
    }
}

struct Pending {
    id: Value,
    config: String,
    value: String,
}

struct Session {
    cmd: UnboundedSender<Command>,
    cwd: PathBuf,
    host: Rc<PluginHost>,
    task: tokio::task::JoinHandle<()>,
    /// Request id of the `session/prompt` in flight.
    prompt: Option<Value>,
    pending_cfg: Option<Pending>,
    models: Vec<String>,
    /// `(model, thinking)` as last sent to the client.
    advertised: (String, String),
    used: Option<u64>,
    title: Option<String>,
    /// Distinguishes reopened sessions that share an id, so a closed agent's late events are dropped.
    epoch: u64,
    providers: Rc<mu_provider::provider::client::Providers>,
}

impl Session {
    fn config_options(&self) -> Value {
        let models: Vec<Value> =
            self.models.iter().map(|m| json!({"value": m, "name": pricing::display_name(m)})).collect();
        let levels: Vec<Value> = Thinking::NAMES.iter().map(|n| json!({"value": n, "name": n})).collect();
        json!([
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": self.advertised.0, "options": models},
            {"id": "thinking", "name": "Thinking", "category": "thought_level", "type": "select", "currentValue": self.advertised.1, "options": levels},
        ])
    }
}

struct Server {
    model: Option<String>,
    thinking: Option<Thinking>,
    /// The client advertised `session.notices`.
    notices: bool,
    sessions: HashMap<String, Session>,
    ev_tx: UnboundedSender<(String, u64, AgentEvent)>,
    next_epoch: u64,
    /// Results of work that runs off the request loop (opening sessions, listing, model lists).
    done_tx: UnboundedSender<Done>,
}

/// What a background task hands back to the request loop.
enum Done {
    Opened {
        id: Value,
        /// `session/new` (answers with the id) as opposed to load/resume.
        new: bool,
        result: Result<Box<Opened>>,
    },
    Models {
        sid: String,
        epoch: u64,
        models: Vec<String>,
    },
    Listed {
        id: Value,
        sessions: Vec<Value>,
    },
}

struct Opened {
    sid: String,
    session: Session,
    ev_rx: UnboundedReceiver<AgentEvent>,
    notice_rx: UnboundedReceiver<String>,
    providers: Rc<mu_provider::provider::client::Providers>,
}

async fn serve(model: Option<String>, thinking: Option<Thinking>) {
    let mut input = spawn_stdin();
    let (ev_tx, mut ev_rx) = unbounded_channel();
    let (done_tx, mut done_rx) = unbounded_channel();
    let mut srv = Server { model, thinking, notices: false, sessions: HashMap::new(), ev_tx, next_epoch: 0, done_tx };
    loop {
        tokio::select! {
            m = input.recv() => match m {
                Some(v) => srv.handle(v).await,
                None => break,
            },
            Some((sid, epoch, e)) = ev_rx.recv() => srv.on_event(&sid, epoch, e),
            Some(d) = done_rx.recv() => srv.on_done(d),
        }
    }
    srv.shutdown().await;
}

fn notice(enabled: bool, sid: &str, severity: &str, msg: String) {
    if enabled {
        update(sid, json!({"sessionUpdate": "notice", "severity": severity, "title": msg}));
    } else {
        eprintln!("{msg}");
    }
}

impl Server {
    async fn handle(&mut self, msg: Value) {
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let Some(method) = msg["method"].as_str() else {
            // A response to something we asked has an id and no method; we ask nothing, so it is
            // ignored. Anything else with an id is a malformed request and gets an answer.
            if let Some(id) = id
                && msg.get("result").is_none()
                && msg.get("error").is_none()
            {
                fail(&id, INVALID_REQUEST, "request has no method");
            }
            return;
        };
        let p = &msg["params"];
        let sid = p["sessionId"].as_str().unwrap_or("");
        match (method, id) {
            ("initialize", Some(id)) => {
                self.notices = p["clientCapabilities"]["session"]["notices"].is_object();
                respond(
                    &id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {"image": true, "embeddedContext": true},
                            "sessionCapabilities": {"list": {}, "resume": {}, "close": {}},
                        },
                        "agentInfo": {"name": "mu", "title": "mu", "version": env!("CARGO_PKG_VERSION")},
                        "authMethods": [],
                    }),
                );
            }
            ("session/new", Some(id)) => {
                let Some(cwd) = self.cwd_param(&id, p) else { return };
                self.spawn_open(id, cwd, None, false, true);
            }
            ("session/list", Some(id)) => {
                let home = std::env::current_dir().unwrap_or_default();
                let want = p["cwd"].as_str().map(PathBuf::from);
                let done = self.done_tx.clone();
                // Sorted by file name and capped before any file is read; off the request loop.
                let work = tokio::task::spawn_blocking(move || {
                    let sessions: Vec<Value> = session::list_recent(&home, want.as_deref(), 500)
                        .into_iter()
                        .filter_map(|s| {
                            let cwd = s.cwd.clone().filter(|c| c.is_absolute())?;
                            let updated =
                                jiff::Timestamp::try_from(s.modified).ok()?.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
                            let title = s.title.or_else(|| Some(s.first_prompt.clone()).filter(|t| !t.is_empty()));
                            Some(json!({"sessionId": session::id_of(&s.path), "cwd": cwd, "title": title, "updatedAt": updated}))
                        })
                        .collect();
                    sessions
                });
                tokio::task::spawn_local(async move {
                    match work.await {
                        Ok(sessions) => {
                            let _ = done.send(Done::Listed { id, sessions });
                        }
                        Err(e) => fail(&id, INTERNAL, format!("listing failed: {e}")),
                    }
                });
            }
            ("session/load" | "session/resume", Some(id)) => {
                let Some(cwd) = self.cwd_param(&id, p) else { return };
                let path = match session::find_id(&cwd, sid) {
                    Ok(p) => p,
                    Err(e) => return fail(&id, INVALID_PARAMS, e),
                };
                if self.sessions.contains_key(&session::id_of(&path)) {
                    return fail(&id, INVALID_REQUEST, "session is already open");
                }
                self.spawn_open(id, cwd, Some(path), method == "session/load", false);
            }
            ("session/prompt", Some(id)) => {
                let Some(s) = self.sessions.get_mut(sid) else { return fail(&id, INVALID_PARAMS, "unknown session") };
                if s.prompt.is_some() {
                    return fail(&id, INVALID_REQUEST, "a prompt is already running");
                }
                s.prompt = Some(id);
                let _ = s.cmd.send(Command::Prompt(user_input(&p["prompt"])));
            }
            ("session/cancel", _) => {
                if let Some(s) = self.sessions.get(sid) {
                    let _ = s.cmd.send(Command::Cancel);
                }
            }
            ("session/set_config_option", Some(id)) => {
                let Some(s) = self.sessions.get_mut(sid) else { return fail(&id, INVALID_PARAMS, "unknown session") };
                if s.prompt.is_some() || s.pending_cfg.is_some() {
                    return fail(&id, INVALID_REQUEST, "session is busy");
                }
                let (config, value) = (p["configId"].as_str().unwrap_or(""), p["value"].as_str().unwrap_or(""));
                let cmd = match config {
                    "model" => {
                        let providers = s.providers.clone();
                        match model_target(value, &s.models, |v| {
                            providers.resolve(v).map(|(m, _)| m.id).map_err(|e| e.to_string())
                        }) {
                            Ok(target) => {
                                s.pending_cfg = Some(Pending { id, config: config.into(), value: target });
                                let _ = s.cmd.send(Command::SetModel(value.into()));
                                return;
                            }
                            Err(e) => return fail(&id, INVALID_PARAMS, e),
                        }
                    }
                    "thinking" => match Thinking::parse(value) {
                        Some(t) => Command::SetThinking(t),
                        None => return fail(&id, INVALID_PARAMS, format!("unknown thinking level {value}")),
                    },
                    _ => return fail(&id, INVALID_PARAMS, format!("unknown config option {config}")),
                };
                s.pending_cfg = Some(Pending { id, config: config.into(), value: value.into() });
                let _ = s.cmd.send(cmd);
            }
            ("session/close", Some(id)) => match self.sessions.remove(sid) {
                Some(mut s) => {
                    let _ = s.cmd.send(Command::Cancel);
                    let _ = s.cmd.send(Command::Exit);
                    if let Some(pid) = s.prompt.take() {
                        respond(&pid, json!({"stopReason": "cancelled"}));
                    }
                    if let Some(p) = s.pending_cfg.take() {
                        fail(&p.id, INTERNAL, "session closed");
                    }
                    tokio::task::spawn_local(async move {
                        let _ = s.task.await;
                        s.host.shutdown_all().await;
                    });
                    respond(&id, json!({}));
                }
                None => fail(&id, INVALID_PARAMS, "unknown session"),
            },
            (m, Some(id)) => fail(&id, METHOD_NOT_FOUND, format!("method not found: {m}")),
            _ => {}
        }
    }

    fn cwd_param(&self, id: &Value, p: &Value) -> Option<PathBuf> {
        let cwd = PathBuf::from(p["cwd"].as_str().unwrap_or(""));
        if cwd.is_absolute() && cwd.is_dir() {
            Some(cwd)
        } else {
            fail(id, INVALID_PARAMS, "cwd must be an absolute path to an existing directory");
            None
        }
    }

    /// Starts opening a session in the background; the reply goes out from `on_done`.
    fn spawn_open(&mut self, id: Value, cwd: PathBuf, resume: Option<PathBuf>, replay: bool, new: bool) {
        self.next_epoch += 1;
        let epoch = self.next_epoch;
        let (model, thinking, done) = (self.model.clone(), self.thinking, self.done_tx.clone());
        tokio::task::spawn_local(async move {
            let result = open_session(model, thinking, epoch, cwd, resume, replay).await;
            let _ = done.send(Done::Opened { id, new, result });
        });
    }

    fn on_done(&mut self, d: Done) {
        match d {
            Done::Listed { id, sessions } => respond(&id, json!({"sessions": sessions})),
            Done::Models { sid, epoch, models } => {
                let Some(s) = self.sessions.get_mut(&sid).filter(|s| s.epoch == epoch) else { return };
                let before = s.models.len();
                for m in models {
                    if !s.models.contains(&m) {
                        s.models.push(m);
                    }
                }
                if s.models.len() != before {
                    update(&sid, json!({"sessionUpdate": "config_option_update", "configOptions": s.config_options()}));
                }
            }
            Done::Opened { id, new, result } => {
                let o = match result {
                    Ok(o) => o,
                    Err(e) => return fail(&id, INTERNAL, e.to_string()),
                };
                let Opened { sid, session, ev_rx, notice_rx, providers } = *o;
                if self.sessions.contains_key(&sid) {
                    // Another request opened it first.
                    let _ = session.cmd.send(Command::Exit);
                    tokio::task::spawn_local(async move {
                        let _ = session.task.await;
                        session.host.shutdown_all().await;
                    });
                    return fail(&id, INVALID_REQUEST, "session is already open");
                }
                let epoch = session.epoch;
                tokio::task::spawn_local(forward(sid.clone(), epoch, ev_rx, self.ev_tx.clone(), |e| e));
                tokio::task::spawn_local(forward(
                    sid.clone(),
                    epoch,
                    notice_rx,
                    self.ev_tx.clone(),
                    AgentEvent::Notice,
                ));
                self.sessions.insert(sid.clone(), session);
                let opts = self.sessions[&sid].config_options();
                respond(
                    &id,
                    if new { json!({"sessionId": sid, "configOptions": opts}) } else { json!({"configOptions": opts}) },
                );
                // The full model list comes from the gateway, which can be slow: fetch it now that
                // the session is usable, and add to the cached list the session started with.
                let done = self.done_tx.clone();
                tokio::task::spawn_local(async move {
                    if let Ok(Ok(rows)) =
                        tokio::time::timeout(Duration::from_secs(10), providers.gateway_models()).await
                    {
                        let models = rows.into_iter().map(|m| m.id).collect();
                        let _ = done.send(Done::Models { sid, epoch, models });
                    }
                });
            }
        }
    }

    fn on_event(&mut self, sid: &str, epoch: u64, e: AgentEvent) {
        if self.sessions.get(sid).is_none_or(|s| s.epoch != epoch) {
            return;
        }
        if matches!(e, AgentEvent::Exit) {
            if let Some(s) = self.sessions.remove(sid) {
                if let Some(id) = s.prompt {
                    fail(&id, INTERNAL, "agent exited");
                }
                if let Some(p) = s.pending_cfg {
                    fail(&p.id, INTERNAL, "agent exited");
                }
            }
            return;
        }
        let notices = self.notices;
        let Some(s) = self.sessions.get_mut(sid) else { return };
        match e {
            AgentEvent::Status(st) => {
                if !s.models.contains(&st.model_id) {
                    s.models.push(st.model_id.clone());
                }
                let cur = (st.model_id.clone(), st.thinking.clone());
                let changed = s.advertised != cur;
                let satisfied = s.pending_cfg.as_ref().is_some_and(|p| {
                    // A model request may resolve to a different id than the one asked for (an alias),
                    // so a model change also answers it; otherwise the request would never be answered.
                    if p.config == "model" { cur.0 == p.value || s.advertised.0 != cur.0 } else { cur.1 == p.value }
                });
                s.advertised = cur;
                if satisfied {
                    let p = s.pending_cfg.take().expect("checked above");
                    respond(&p.id, json!({"configOptions": s.config_options()}));
                } else if changed {
                    update(sid, json!({"sessionUpdate": "config_option_update", "configOptions": s.config_options()}));
                }
                if let Some(used) = st.context_tokens.filter(|_| st.context_window > 0)
                    && s.used != Some(used)
                {
                    update(sid, json!({"sessionUpdate": "usage_update", "used": used, "size": st.context_window}));
                }
                s.used = st.context_tokens;
                if st.title != s.title {
                    if let Some(t) = &st.title {
                        update(sid, json!({"sessionUpdate": "session_info_update", "title": t}));
                    }
                    s.title = st.title;
                }
            }
            AgentEvent::TurnEnd { aborted } => {
                if let Some(id) = s.prompt.take() {
                    respond(&id, json!({"stopReason": if aborted { "cancelled" } else { "end_turn" }}));
                }
            }
            AgentEvent::Error(m) => {
                if let Some(p) = s.pending_cfg.take() {
                    fail(&p.id, INVALID_PARAMS, m);
                } else {
                    eprintln!("{m}");
                    update(
                        sid,
                        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("**Error:** {m}\n\n")}}),
                    );
                }
            }
            AgentEvent::Notice(m) => notice(notices, sid, "info", m),
            AgentEvent::Retry { attempt, delay, reason } => {
                notice(notices, sid, "warning", mu_base::events::retry_notice(attempt, delay, &reason))
            }
            AgentEvent::Compacted { before, after } => {
                notice(notices, sid, "info", format!("compacted {before}→{after} tokens"))
            }
            e => {
                for u in translate(&s.cwd, e, true) {
                    update(sid, u);
                }
            }
        }
    }

    async fn shutdown(&mut self) {
        let sessions: Vec<Session> = self.sessions.drain().map(|(_, s)| s).collect();
        for s in &sessions {
            let _ = s.cmd.send(Command::Cancel);
            let _ = s.cmd.send(Command::Exit);
        }
        for s in sessions {
            let _ = s.task.await;
            s.host.shutdown_all().await;
        }
    }
}

/// Starts an agent for `cwd` and waits until its session file exists. Replayed history, if
/// requested, is streamed before this returns. Runs off the request loop.
async fn open_session(
    model: Option<String>,
    thinking: Option<Thinking>,
    epoch: u64,
    cwd: PathBuf,
    resume: Option<PathBuf>,
    replay: bool,
) -> Result<Box<Opened>> {
    let Setup { cfgs, ctx, host, providers, notice_rx, .. } = setup(&cwd);
    let (spec, level) = pick_model(&cfgs, &providers, model.clone(), thinking)?;
    let run = agent::RunConfig {
        pin_model: model.is_some() && resume.is_some(),
        pin_thinking: thinking.is_some() && resume.is_some(),
        excluded: agent::HEADLESS_EXCLUDED.to_vec(),
        ..Default::default()
    };
    let AgentHandle { cmd: cmd_tx, ev: mut ev_rx, task, .. } = build_agent(
        AgentSetup {
            providers: providers.clone(),
            plugins: host.clone(),
            ctx,
            model: spec,
            thinking: level,
            deliver: crate::workflow::deliver::detached_starter(),
            run,
            output_schema: None,
        },
        cwd.clone(),
        Mode::Acp,
    );
    let known = resume.as_deref().map(session::id_of);
    let _ = cmd_tx.send(match resume {
        Some(p) => Command::Resume(p),
        None => Command::EnsureSession,
    });

    let ready = loop {
        match ev_rx.recv().await {
            Some(AgentEvent::Status(s)) if s.session_path.is_some() => break Ok(s),
            Some(AgentEvent::Error(e)) => break Err(anyhow!(e)),
            Some(AgentEvent::Exit) | None => break Err(anyhow!("agent exited during startup")),
            Some(e) => {
                if let (true, Some(sid)) = (replay, &known) {
                    for u in translate(&cwd, e, false) {
                        update(sid, u);
                    }
                }
            }
        }
    };
    let st = match ready {
        Ok(st) => st,
        Err(e) => {
            let _ = cmd_tx.send(Command::Exit);
            let _ = task.await;
            host.shutdown_all().await;
            return Err(e);
        }
    };
    let sid = known.unwrap_or_else(|| session::id_of(Path::new(st.session_path.as_deref().unwrap_or(""))));
    // Start from the cached list: no network wait before the session is usable.
    let mut models: Vec<String> = providers.cached_models().into_iter().map(|m| m.id).collect();
    if !models.contains(&st.model_id) {
        models.push(st.model_id.clone());
    }
    let session = Session {
        cmd: cmd_tx,
        cwd,
        host,
        task,
        prompt: None,
        pending_cfg: None,
        models,
        advertised: (st.model_id, st.thinking),
        providers: providers.clone(),
        used: st.context_tokens,
        title: st.title,
        epoch,
    };
    Ok(Box::new(Opened { sid, session, ev_rx, notice_rx, providers }))
}

async fn forward<T: 'static>(
    sid: String,
    epoch: u64,
    mut rx: UnboundedReceiver<T>,
    tx: UnboundedSender<(String, u64, AgentEvent)>,
    f: fn(T) -> AgentEvent,
) {
    while let Some(e) = rx.recv().await {
        if tx.send((sid.clone(), epoch, f(e))).is_err() {
            break;
        }
    }
}

fn user_input(blocks: &Value) -> UserInput {
    let mut parts: Vec<String> = Vec::new();
    let mut images = Vec::new();
    for b in blocks.as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => parts.push(b["text"].as_str().unwrap_or("").into()),
            Some("image") => images.push(mu_base::image::externalize(Image {
                data: b["data"].as_str().unwrap_or("").into(),
                mime_type: b["mimeType"].as_str().unwrap_or("").into(),
            })),
            Some("resource") => {
                let r = &b["resource"];
                let uri = r["uri"].as_str().unwrap_or("");
                parts.push(match r["text"].as_str() {
                    Some(t) => format!("<resource uri=\"{uri}\">\n{t}\n</resource>"),
                    None => format!("[binary resource {uri}]"),
                });
            }
            Some("resource_link") => {
                let uri = b["uri"].as_str().unwrap_or("");
                parts.push(format!("[{}]({uri})", b["name"].as_str().unwrap_or(uri)));
            }
            _ => {}
        }
    }
    UserInput { text: parts.join("\n\n"), images }
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" => "read",
        "grep" | "glob" => "search",
        "edit" | "write" => "edit",
        "bash" => "execute",
        "fetch" | "web_search" => "fetch",
        _ => "other",
    }
}

fn plan_entries(phases: &Value) -> Vec<Value> {
    let phases = phases.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut out = Vec::new();
    for ph in phases {
        for t in ph["tasks"].as_array().into_iter().flatten() {
            let mut content = t["content"].as_str().unwrap_or("").to_string();
            if phases.len() > 1 {
                content = format!("{}: {content}", ph["name"].as_str().unwrap_or(""));
            }
            let status = match t["status"].as_str() {
                Some("in_progress") => "in_progress",
                Some("completed") => "completed",
                Some("abandoned") => {
                    content.push_str(" (abandoned)");
                    "completed"
                }
                Some("blocked") => {
                    match t["blocker"].as_str() {
                        Some(b) => content.push_str(&format!(" (blocked: {b})")),
                        None => content.push_str(" (blocked)"),
                    }
                    "pending"
                }
                _ => "pending",
            };
            out.push(json!({"content": content, "priority": "medium", "status": status}));
        }
    }
    out
}

/// ACP `session/update` payloads for one agent event. `live` is false while replaying history.
fn translate(cwd: &Path, e: AgentEvent, live: bool) -> Vec<Value> {
    let text = |kind: &str, t: String| json!({"sessionUpdate": kind, "content": {"type": "text", "text": t}});
    match e {
        AgentEvent::UserPrompt(t) if !live => vec![text("user_message_chunk", t)],
        AgentEvent::Text(t) => vec![text("agent_message_chunk", t)],
        AgentEvent::Thinking(t) => vec![text("agent_thought_chunk", t)],
        AgentEvent::ToolStart { id, name, args } => {
            let s = summary(&name, &args);
            let title = if s.is_empty() { name.clone() } else { format!("{name}: {s}") };
            let mut call = json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": title,
                "kind": tool_kind(&name),
                "status": "in_progress",
                "rawInput": args,
            });
            if let (true, Some(p)) = (matches!(name.as_str(), "read" | "write"), args["path"].as_str()) {
                call["locations"] = json!([{"path": cwd.join(p)}]);
            }
            vec![call]
        }
        AgentEvent::ToolEnd { id, output, .. } => {
            let mut u = json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": if output.is_error { "failed" } else { "completed" }});
            if !output.text.is_empty() {
                u["content"] = json!([{"type": "content", "content": {"type": "text", "text": output.text}}]);
            }
            vec![u]
        }
        AgentEvent::Todo(v) => {
            let entries = plan_entries(&v);
            if entries.is_empty() && !live {
                vec![]
            } else {
                vec![json!({"sessionUpdate": "plan", "entries": entries})]
            }
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::tool::ToolOutput;

    fn kinds(v: &[Value]) -> Vec<&str> {
        v.iter().map(|u| u["sessionUpdate"].as_str().unwrap()).collect()
    }

    #[test]
    fn history_replays_user_text_but_live_prompts_are_not_echoed() {
        let cwd = Path::new("/w");
        assert_eq!(kinds(&translate(cwd, AgentEvent::UserPrompt("hi".into()), false)), ["user_message_chunk"]);
        assert!(translate(cwd, AgentEvent::UserPrompt("hi".into()), true).is_empty());
    }

    #[test]
    fn tool_call_carries_kind_title_and_absolute_location() {
        let u = translate(
            Path::new("/w"),
            AgentEvent::ToolStart { id: "c1".into(), name: "read".into(), args: json!({"path": "a/b.rs"}) },
            true,
        );
        assert_eq!(u[0]["kind"], "read");
        assert_eq!(u[0]["title"], "read: a/b.rs");
        assert_eq!(u[0]["locations"][0]["path"], "/w/a/b.rs");
        let u = translate(
            Path::new("/w"),
            AgentEvent::ToolStart { id: "c2".into(), name: "bash".into(), args: json!({"command": "ls"}) },
            true,
        );
        assert_eq!((u[0]["kind"].as_str(), u[0].get("locations")), (Some("execute"), None));
    }

    #[test]
    fn failed_tool_maps_to_failed_status() {
        let out = ToolOutput::err("boom");
        let u =
            translate(Path::new("/w"), AgentEvent::ToolEnd { id: "c1".into(), name: "bash".into(), output: out }, true);
        assert_eq!(
            (u[0]["status"].as_str(), u[0]["content"][0]["content"]["text"].as_str()),
            (Some("failed"), Some("boom"))
        );
    }

    #[test]
    fn todo_becomes_plan_and_empty_replay_is_skipped() {
        let phases = json!([
            {"name": "A", "tasks": [{"content": "x", "status": "in_progress"}, {"content": "y", "status": "blocked", "blocker": "dep"}]},
            {"name": "B", "tasks": [{"content": "z", "status": "abandoned"}]},
        ]);
        let u = translate(Path::new("/w"), AgentEvent::Todo(phases), true);
        let e = u[0]["entries"].as_array().unwrap();
        let got: Vec<(&str, &str)> =
            e.iter().map(|x| (x["content"].as_str().unwrap(), x["status"].as_str().unwrap())).collect();
        assert_eq!(
            got,
            [("A: x", "in_progress"), ("A: y (blocked: dep)", "pending"), ("B: z (abandoned)", "completed")]
        );
        assert!(translate(Path::new("/w"), AgentEvent::Todo(json!([])), false).is_empty());
        assert_eq!(translate(Path::new("/w"), AgentEvent::Todo(json!([])), true).len(), 1);
    }

    #[test]
    fn prompt_blocks_flatten_to_text_and_images() {
        let i = user_input(&json!([
            {"type": "text", "text": "look"},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
            {"type": "resource_link", "uri": "file:///b", "name": "b"},
            {"type": "image", "data": "AAA", "mimeType": "image/png"},
        ]));
        assert_eq!(i.text, "look\n\n<resource uri=\"file:///a\">\nbody\n</resource>\n\n[b](file:///b)");
        assert_eq!((i.images.len(), i.images[0].mime_type.as_str()), (1, "image/png"));
    }

    #[test]
    fn session_ids_cannot_escape_the_session_dir() {
        assert!(session::valid_id("0198abcd"));
        assert!(!session::valid_id(""));
        assert!(!session::valid_id("../x"));
    }

    #[test]
    fn model_target_resolves_roles_up_front_and_rejects_unknown_values() {
        let models = vec!["a/m".to_string()];
        let resolve = |s: &str| match s {
            "@slow" | "@slow:high" => Ok("b/n".to_string()),
            _ => Err(format!("model role {s} is not configured")),
        };
        assert_eq!(model_target("a/m", &models, resolve), Ok("a/m".into()));
        assert_eq!(model_target("@slow", &models, resolve), Ok("b/n".into()));
        assert_eq!(model_target("@slow:high", &models, resolve), Ok("b/n".into()));
        assert!(model_target("@nope", &models, resolve).unwrap_err().contains("not configured"));
        assert_eq!(model_target("x/y", &models, resolve), Err("unknown model x/y".into()));
    }
}
