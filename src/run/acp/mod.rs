//! `--acp`: Agent Client Protocol server over stdio (newline-delimited JSON-RPC).
//! One mu `Agent` per ACP session, so each session keeps its own working directory.

mod client;
mod commands;
mod updates;

use crate::cli::select::pick_model;
use crate::run::setup::{self, Setup};
use anyhow::{Result, anyhow};
use client::Rpc;
use commands::Parsed;
use mu_agent::agent;
use mu_agent::handle::{AgentHandle, AgentSetup, Mode, build_agent};
use mu_base::events::{AgentEvent, Command};
use mu_base::message::{Image, UserInput};
use mu_base::models::Thinking;
use mu_base::permissions::PermissionMode;
use mu_base::roles;
use mu_provider::model::pricing;
use mu_session::session::list;
use mu_session::session::owner::{self, Owner};
use mu_tools::context::Context;
use mu_tools::plugin::host::PluginHost;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use updates::{Jobs, Stream, translate_with};

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL: i64 = -32603;
/// Sessions per `session/list` page.
const PAGE: usize = 100;

pub fn run(model: Option<String>, thinking: Option<Thinking>, permissions: Option<PermissionMode>) -> Result<i32> {
    mu_session::session::owner::set_run_info(mu_session::session::owner::RunKind::Acp, None);
    let rt = mu_base::runtime::current_thread_runtime()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async {
        mu_tools::plugin::process::exit_on_signals();
        serve(model, thinking, permissions).await
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

/// A `session/set_config_option` or `session/set_mode` waiting for the agent to confirm.
struct Pending {
    id: Value,
    /// `model`, `thinking`, `mode` (answers with the options) or `set_mode` (answers with `{}`).
    config: String,
    value: String,
}

/// The modes a session offers: id, name, description. `default` is no mode; the rest are the `command` modes.
fn modes(ctx: &Context) -> Vec<(String, String, String)> {
    let mut out = vec![("default".to_string(), "Default".to_string(), "Read, edit and run commands".to_string())];
    out.extend(ctx.modes.iter().filter(|m| m.command).map(|m| (m.name.clone(), m.title(), m.description.clone())));
    out
}

fn mode_id(mode: Option<&str>) -> &str {
    mode.unwrap_or("default")
}

/// What the client was last told about the session.
#[derive(Clone, PartialEq)]
struct Advertised {
    model: String,
    thinking: String,
    mode: Option<String>,
}

struct Session {
    cmd: UnboundedSender<Command>,
    cwd: PathBuf,
    ctx: Rc<Context>,
    host: Rc<PluginHost>,
    task: tokio::task::JoinHandle<()>,
    /// Request id of the `session/prompt` in flight.
    prompt: Option<Value>,
    /// The turn ended on a fatal error, and the message of the last error.
    failed: bool,
    last_error: Option<String>,
    /// Reply text has streamed since the last block or turn ended: a retry then starts a visibly new reply.
    streamed: bool,
    pending_cfg: Option<Pending>,
    models: Vec<String>,
    advertised: Advertised,
    used: Option<u64>,
    title: Option<String>,
    /// Output of running tools and jobs, and the jobs the client has been told about.
    stream: Stream,
    jobs: Jobs,
    /// The modes' exit tools.
    mode_tools: Vec<String>,
    /// The exit tool call in flight, which the review request refers to.
    plan_call: Option<String>,
    /// Distinguishes reopened sessions that share an id, so a closed agent's late events are dropped.
    epoch: u64,
    providers: Rc<mu_provider::provider::client::Providers>,
}

impl Session {
    fn config_options(&self) -> Value {
        // The configured roles come first, each saying what it is for; choosing one switches to
        // the model it names, as `--model @role` does.
        let agents: Vec<(&str, &[String])> =
            self.ctx.agents.iter().map(|a| (a.name.as_str(), a.model.as_slice())).collect();
        let mut models: Vec<Value> = roles::views(&self.providers.cfg, &agents)
            .into_iter()
            .filter_map(|r| {
                let v = r.value.as_ref()?;
                let name = format!("@{}", r.name);
                Some(json!({"value": name, "name": name, "description": format!("{} · {v}", r.purpose)}))
            })
            .collect();
        models.extend(self.models.iter().map(|m| json!({"value": m, "name": pricing::display_name(m)})));
        let levels: Vec<Value> = Thinking::NAMES.iter().map(|n| json!({"value": n, "name": n})).collect();
        let modes: Vec<Value> =
            modes(&self.ctx).iter().map(|(id, name, d)| json!({"value": id, "name": name, "description": d})).collect();
        let current = mode_id(self.advertised.mode.as_deref());
        json!([
            {"id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": current, "options": modes},
            {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": self.advertised.model, "options": models},
            {"id": "thinking", "name": "Thinking", "category": "thought_level", "type": "select", "currentValue": self.advertised.thinking, "options": levels},
        ])
    }

    fn modes(&self) -> Value {
        let available: Vec<Value> =
            modes(&self.ctx).iter().map(|(id, name, d)| json!({"id": id, "name": name, "description": d})).collect();
        json!({"currentModeId": mode_id(self.advertised.mode.as_deref()), "availableModes": available})
    }

    fn command_list(&self) -> Value {
        json!({"sessionUpdate": "available_commands_update", "availableCommands": commands::available(&self.ctx)})
    }
}

struct Server {
    model: Option<String>,
    thinking: Option<Thinking>,
    /// `--permissions`, applied to every session this server opens.
    permissions: Option<PermissionMode>,
    /// The client advertised `session.notices`.
    notices: bool,
    /// The client can show an `elicitation/create` form, so the model may use `ask`.
    elicit: bool,
    sessions: HashMap<String, Session>,
    ev_tx: UnboundedSender<(String, u64, AgentEvent)>,
    next_epoch: u64,
    /// Results of work that runs in its own task instead of inline in the request loop (opening sessions,
    /// listing, model lists).
    done_tx: UnboundedSender<Done>,
    /// Requests the agent makes of the client.
    rpc: Rpc,
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
        next: Option<String>,
    },
}

struct Opened {
    sid: String,
    session: Session,
    ev_rx: UnboundedReceiver<AgentEvent>,
    notice_rx: UnboundedReceiver<String>,
    providers: Rc<mu_provider::provider::client::Providers>,
}

async fn serve(model: Option<String>, thinking: Option<Thinking>, permissions: Option<PermissionMode>) {
    let mut input = spawn_stdin();
    let (ev_tx, mut ev_rx) = unbounded_channel();
    let (done_tx, mut done_rx) = unbounded_channel();
    let mut srv = Server {
        model,
        thinking,
        permissions,
        notices: false,
        elicit: false,
        sessions: HashMap::new(),
        ev_tx,
        next_epoch: 0,
        done_tx,
        rpc: Rpc::new(Rc::new(send)),
    };
    loop {
        let due = srv.next_flush();
        tokio::select! {
            m = input.recv() => match m {
                Some(v) => srv.handle(v).await,
                None => break,
            },
            Some((sid, epoch, e)) = ev_rx.recv() => srv.on_event(&sid, epoch, e),
            Some(d) = done_rx.recv() => srv.on_done(d),
            () = async {
                match due {
                    Some(t) => tokio::time::sleep_until(t.into()).await,
                    None => std::future::pending().await,
                }
            } => srv.flush_due(),
        }
    }
    srv.shutdown().await;
}

/// A notice to the user. A client that takes `notice` updates gets one; any other gets the text
/// as an italic line of the reply while a prompt runs (a notice must not be lost, and outside a
/// prompt there is no reply to put it in), and stderr always.
fn notice(enabled: bool, in_turn: bool, sid: &str, severity: &str, msg: String) {
    if enabled {
        update(sid, json!({"sessionUpdate": "notice", "severity": severity, "title": msg}));
        return;
    }
    eprintln!("{msg}");
    if in_turn {
        update(
            sid,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("\n\n*{msg}*\n\n")}}),
        );
    }
}

impl Server {
    async fn handle(&mut self, msg: Value) {
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let Some(method) = msg["method"].as_str() else {
            // A response to a request of ours (plan approval, a question) has an id and no method.
            // Anything else with an id is a malformed request and gets an answer.
            if let Some(id) = id {
                if msg.get("result").is_some() || msg.get("error").is_some() {
                    let reply = match msg.get("result") {
                        Some(r) => Ok(r.clone()),
                        None => Err(msg["error"].clone()),
                    };
                    self.rpc.resolve(&id, reply);
                } else {
                    fail(&id, INVALID_REQUEST, "request has no method");
                }
            }
            return;
        };
        let p = &msg["params"];
        let sid = p["sessionId"].as_str().unwrap_or("");
        match (method, id) {
            ("initialize", Some(id)) => {
                // Only v1 is spoken, and the answer names it whatever version was asked for, 0 included.
                if p["protocolVersion"].as_u64().is_none() {
                    return fail(&id, INVALID_PARAMS, "protocolVersion must be a non-negative integer");
                }
                self.notices = p["clientCapabilities"]["session"]["notices"].is_object();
                self.elicit = p["clientCapabilities"]["elicitation"]["form"].is_object();
                respond(
                    &id,
                    json!({
                        "protocolVersion": 1,
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {"image": true, "embeddedContext": true},
                            "sessionCapabilities": {"list": {}, "resume": {}, "close": {}, "delete": {}},
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
                let want = match p.get("cwd").filter(|c| !c.is_null()) {
                    None => None,
                    Some(c) => match c.as_str().map(PathBuf::from).filter(|c| c.is_absolute()) {
                        Some(c) => Some(c),
                        None => return fail(&id, INVALID_PARAMS, "cwd must be an absolute path"),
                    },
                };
                let offset = match p.get("cursor").filter(|c| !c.is_null()) {
                    None => 0,
                    Some(c) => match c.as_str().and_then(|s| s.parse::<usize>().ok()) {
                        Some(o) => o,
                        None => return fail(&id, INVALID_PARAMS, "invalid cursor"),
                    },
                };
                let home = std::env::current_dir().unwrap_or_default();
                let done = self.done_tx.clone();
                // Newest first by file name; only as many files as the page needs are read. Off the request loop.
                let work = tokio::task::spawn_blocking(move || {
                    let total: Vec<Value> = list::list_recent(&home, want.as_deref(), offset.saturating_add(PAGE + 1))
                        .into_iter()
                        .filter_map(|s| {
                            let cwd = s.cwd.clone().filter(|c| c.is_absolute())?;
                            let updated =
                                jiff::Timestamp::try_from(s.modified).ok()?.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
                            let title = s.title.or_else(|| Some(s.first_prompt.clone()).filter(|t| !t.is_empty()));
                            Some(json!({"sessionId": list::id_of(&s.path), "cwd": cwd, "title": title, "updatedAt": updated}))
                        })
                        .collect();
                    page(total, offset)
                });
                tokio::task::spawn_local(async move {
                    match work.await {
                        Ok((sessions, next)) => {
                            let _ = done.send(Done::Listed { id, sessions, next });
                        }
                        Err(e) => fail(&id, INTERNAL, format!("listing failed: {e}")),
                    }
                });
            }
            ("session/load" | "session/resume", Some(id)) => {
                let Some(cwd) = self.cwd_param(&id, p) else { return };
                let path = match list::find_id(&cwd, sid) {
                    Ok(p) => p,
                    Err(e) => return fail(&id, INVALID_PARAMS, e.to_string()),
                };
                if self.sessions.contains_key(&list::id_of(&path)) {
                    return fail(&id, INVALID_REQUEST, "session is already open");
                }
                self.spawn_open(id, cwd, Some(path), method == "session/load", false);
            }
            ("session/prompt", Some(id)) => self.prompt(id, sid, &p["prompt"]),
            ("session/cancel", _) => {
                if let Some(s) = self.sessions.get(sid) {
                    let _ = s.cmd.send(Command::Cancel);
                }
            }
            // The prompt answers with `cancelled`, which a cancelled request may do.
            ("$/cancel_request", _) => {
                if let Some(s) = self.sessions.values().find(|s| s.prompt.as_ref() == p.get("requestId")) {
                    let _ = s.cmd.send(Command::Cancel);
                }
            }
            ("session/set_config_option", Some(id)) => {
                let (config, value) = (p["configId"].as_str().unwrap_or(""), p["value"].as_str().unwrap_or(""));
                self.set_config(id, sid, config, value, config);
            }
            ("session/set_mode", Some(id)) => {
                self.set_config(id, sid, "mode", p["modeId"].as_str().unwrap_or(""), "set_mode");
            }
            ("session/close", Some(id)) => {
                if self.close(sid) {
                    respond(&id, json!({}));
                } else {
                    fail(&id, INVALID_PARAMS, "unknown session");
                }
            }
            // Gone from `session/list`: the file is removed. A session that is not there is deleted already.
            ("session/delete", Some(id)) => {
                self.close(sid);
                let cwd = std::env::current_dir().unwrap_or_default();
                match list::find_id(&cwd, sid) {
                    Ok(path) => {
                        // This server's own session is closed above; anything else open is someone else's.
                        match owner::owner(&path) {
                            Owner::Free => {}
                            Owner::Live { pid, .. } if pid == std::process::id() => {}
                            Owner::Live { pid, .. } => {
                                return fail(&id, INVALID_REQUEST, format!("session is open in process {pid}"));
                            }
                            Owner::Unknown => return fail(&id, INVALID_REQUEST, "session is in use"),
                        }
                        match std::fs::remove_file(&path) {
                            Ok(()) => respond(&id, json!({})),
                            Err(e) => fail(&id, INTERNAL, format!("cannot delete the session: {e}")),
                        }
                    }
                    Err(_) => respond(&id, json!({})),
                }
            }
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

    /// `session/prompt`: a command at the start of the text runs as one, anything else goes to the model.
    fn prompt(&mut self, id: Value, sid: &str, blocks: &Value) {
        let Some(s) = self.sessions.get_mut(sid) else { return fail(&id, INVALID_PARAMS, "unknown session") };
        if s.prompt.is_some() {
            return fail(&id, INVALID_REQUEST, "a prompt is already running");
        }
        let input = user_input(blocks);
        let end_turn = |id: &Value| respond(id, json!({"stopReason": "end_turn"}));
        match commands::parse(&s.ctx, &input.text) {
            Parsed::Plain => {
                s.prompt = Some(id);
                let _ = s.cmd.send(Command::Prompt(input));
            }
            Parsed::Prompt(text) => {
                s.prompt = Some(id);
                let command = Some(input.text.trim().to_string());
                let _ = s.cmd.send(Command::Prompt(UserInput { text, images: input.images, command }));
            }
            Parsed::Compact(focus) => {
                s.prompt = Some(id);
                let _ = s.cmd.send(Command::Compact(focus));
            }
            Parsed::Review(focus) => {
                s.prompt = Some(id);
                let _ = s.cmd.send(Command::Review(focus));
            }
            Parsed::Mode { name, prompt: Some(text) } => {
                s.prompt = Some(id);
                let _ = s.cmd.send(Command::Mode { name, prompt: Some(text) });
            }
            Parsed::Mode { name, prompt: None } => {
                let _ = s.cmd.send(Command::Mode { name, prompt: None });
                end_turn(&id);
            }
            Parsed::Rename(title) => {
                let _ = s.cmd.send(Command::Rename(title));
                end_turn(&id);
            }
            Parsed::Reply(text) => {
                update(
                    sid,
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("{text}\n")}}),
                );
                end_turn(&id);
            }
        }
    }

    /// `session/set_config_option` (and `session/set_mode`, as `mode`): applies the option, and answers
    /// once the agent reports it. `reply` is the pending request kind.
    fn set_config(&mut self, id: Value, sid: &str, config: &str, value: &str, reply: &str) {
        let Some(s) = self.sessions.get_mut(sid) else { return fail(&id, INVALID_PARAMS, "unknown session") };
        if s.prompt.is_some() || s.pending_cfg.is_some() {
            return fail(&id, INVALID_REQUEST, "session is busy");
        }
        let cmd = match config {
            "model" => {
                let providers = s.providers.clone();
                match model_target(value, &s.models, |v| {
                    providers.resolve(v).map(|(m, _)| m.id).map_err(|e| e.to_string())
                }) {
                    Ok(target) => {
                        s.pending_cfg = Some(Pending { id, config: reply.into(), value: target });
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
            "mode" => {
                if !modes(&s.ctx).iter().any(|m| m.0 == value) {
                    return fail(&id, INVALID_PARAMS, format!("unknown mode {value}"));
                }
                if value == mode_id(s.advertised.mode.as_deref()) {
                    return respond(
                        &id,
                        if reply == "set_mode" { json!({}) } else { json!({"configOptions": s.config_options()}) },
                    );
                }
                if value == "default" { Command::ExitMode } else { Command::Mode { name: value.into(), prompt: None } }
            }
            _ => return fail(&id, INVALID_PARAMS, format!("unknown config option {config}")),
        };
        s.pending_cfg = Some(Pending { id, config: reply.into(), value: value.into() });
        let _ = s.cmd.send(cmd);
    }

    /// Ends a session this server has open. `false` when there is none by that id.
    fn close(&mut self, sid: &str) -> bool {
        let Some(mut s) = self.sessions.remove(sid) else { return false };
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
        true
    }

    /// Starts opening a session in the background; the reply goes out from `on_done`.
    fn spawn_open(&mut self, id: Value, cwd: PathBuf, resume: Option<PathBuf>, replay: bool, new: bool) {
        self.next_epoch += 1;
        let epoch = self.next_epoch;
        let (model, thinking, done, elicit) = (self.model.clone(), self.thinking, self.done_tx.clone(), self.elicit);
        let permissions = self.permissions;
        tokio::task::spawn_local(async move {
            let result = open_session(model, thinking, permissions, epoch, cwd, resume, replay, elicit).await;
            let _ = done.send(Done::Opened { id, new, result });
        });
    }

    fn on_done(&mut self, d: Done) {
        match d {
            Done::Listed { id, sessions, next } => match next {
                Some(c) => respond(&id, json!({"sessions": sessions, "nextCursor": c})),
                None => respond(&id, json!({"sessions": sessions})),
            },
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
                tokio::task::spawn_local(forward_events(sid.clone(), epoch, ev_rx, self.ev_tx.clone()));
                tokio::task::spawn_local(forward(
                    sid.clone(),
                    epoch,
                    notice_rx,
                    self.ev_tx.clone(),
                    AgentEvent::Notice,
                ));
                self.sessions.insert(sid.clone(), session);
                let s = &self.sessions[&sid];
                let (opts, modes) = (s.config_options(), s.modes());
                respond(
                    &id,
                    if new {
                        json!({"sessionId": sid, "configOptions": opts, "modes": modes})
                    } else {
                        json!({"configOptions": opts, "modes": modes})
                    },
                );
                update(&sid, s.command_list());
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
        let (notices, rpc, elicit) = (self.notices, self.rpc.clone(), self.elicit);
        let Some(s) = self.sessions.get_mut(sid) else { return };
        match &e {
            AgentEvent::Text(_) => s.streamed = true,
            AgentEvent::BlockEnd | AgentEvent::TurnEnd { .. } => s.streamed = false,
            _ => {}
        }
        match e {
            AgentEvent::Status(st) => {
                if !s.models.contains(&st.model_id) {
                    s.models.push(st.model_id.clone());
                }
                let cur =
                    Advertised { model: st.model_id.clone(), thinking: st.thinking.clone(), mode: st.mode.clone() };
                let changed = s.advertised != cur;
                let mode_changed = s.advertised.mode != cur.mode;
                let satisfied = s.pending_cfg.as_ref().is_some_and(|p| match p.config.as_str() {
                    // A model request may resolve to a different id than the one asked for (an alias),
                    // so a model change also answers it; otherwise the request would never be answered.
                    "model" => cur.model == p.value || s.advertised.model != cur.model,
                    "thinking" => cur.thinking == p.value,
                    _ => mode_id(cur.mode.as_deref()) == p.value,
                });
                s.advertised = cur;
                if satisfied {
                    let p = s.pending_cfg.take().expect("checked above");
                    respond(
                        &p.id,
                        if p.config == "set_mode" { json!({}) } else { json!({"configOptions": s.config_options()}) },
                    );
                } else if changed {
                    update(sid, json!({"sessionUpdate": "config_option_update", "configOptions": s.config_options()}));
                }
                if mode_changed {
                    update(
                        sid,
                        json!({
                            "sessionUpdate": "current_mode_update",
                            "currentModeId": mode_id(s.advertised.mode.as_deref()),
                        }),
                    );
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
            AgentEvent::TurnFailed => s.failed = true,
            AgentEvent::TurnEnd { aborted } => {
                s.stream.clear();
                s.plan_call = None;
                let failed = std::mem::take(&mut s.failed);
                let reason = s.last_error.take();
                if let Some(id) = s.prompt.take() {
                    if failed {
                        fail(&id, INTERNAL, reason.unwrap_or_else(|| "the turn failed".into()));
                    } else {
                        respond(&id, json!({"stopReason": if aborted { "cancelled" } else { "end_turn" }}));
                    }
                }
            }
            AgentEvent::Error(m) => {
                s.last_error = Some(m.clone());
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
            AgentEvent::Notice(m) => notice(notices, s.prompt.is_some(), sid, "info", m),
            AgentEvent::Retry { attempt, max_attempts, delay, reason } => {
                if std::mem::take(&mut s.streamed) {
                    update(
                        sid,
                        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "\n\n— retrying; the reply starts over below —\n\n"}}),
                    );
                }
                notice(
                    notices,
                    s.prompt.is_some(),
                    sid,
                    "warning",
                    mu_base::events::retry_notice(attempt, max_attempts, delay, &reason),
                )
            }
            AgentEvent::Compacted { before, after } => {
                notice(notices, s.prompt.is_some(), sid, "info", format!("compacted {before}→{after} tokens"))
            }
            AgentEvent::ToolOutput { id, chunk } => {
                if let Some(u) = s.stream.append(&id, &chunk, &s.cwd) {
                    update(sid, u);
                }
            }
            AgentEvent::ToolStart { id, name, args } => {
                if s.mode_tools.contains(&name) {
                    s.plan_call = Some(id.clone());
                }
                for u in translate_with(&s.cwd, AgentEvent::ToolStart { id, name, args }, true, &s.mode_tools) {
                    update(sid, u);
                }
            }
            AgentEvent::ToolEnd { id, name, output } => {
                s.stream.end(&id);
                for u in translate_with(&s.cwd, AgentEvent::ToolEnd { id, name, output }, true, &s.mode_tools) {
                    update(sid, u);
                }
            }
            AgentEvent::Jobs(list) => {
                for u in s.jobs.update(&list, &mut s.stream, &s.cwd) {
                    update(sid, u);
                }
            }
            AgentEvent::JobOutput { id, tail } => {
                if let Some(u) = s.jobs.output(&id, tail, &mut s.stream, &s.cwd) {
                    update(sid, u);
                }
            }
            AgentEvent::Approve { request, reply } => {
                tokio::task::spawn_local(client::approve_tool(rpc, sid.to_string(), request, reply));
            }
            AgentEvent::Ask { questions, reply } => {
                // `ask` is only offered when the client takes forms, so this is the form case; a
                // dropped reply is a cancelled question.
                if elicit {
                    tokio::task::spawn_local(client::ask_user(rpc, sid.to_string(), questions, reply));
                }
            }
            AgentEvent::Review { request, reply } => {
                let call = s.plan_call.take().unwrap_or_else(|| "plan-review".to_string());
                tokio::task::spawn_local(client::review(rpc, sid.to_string(), call, request, reply));
            }
            e => {
                for u in translate_with(&s.cwd, e, true, &s.mode_tools) {
                    update(sid, u);
                }
            }
        }
    }

    /// When the next held-back tool output falls due.
    fn next_flush(&self) -> Option<Instant> {
        self.sessions.values().filter_map(|s| s.stream.next_due()).min()
    }

    fn flush_due(&mut self) {
        let now = Instant::now();
        for (sid, s) in &mut self.sessions {
            for u in s.stream.due(now, &s.cwd) {
                update(sid, u);
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
/// requested, is streamed before this returns. Runs as its own local task, so the request loop keeps
/// turning while it waits; the disk reads of `setup` run on a blocking thread, and only building
/// its `Rc` state is done here. `elicit`: the client can ask the user a form, so the model may use `ask`.
#[allow(clippy::too_many_arguments)]
async fn open_session(
    model: Option<String>,
    thinking: Option<Thinking>,
    permissions: Option<PermissionMode>,
    epoch: u64,
    cwd: PathBuf,
    resume: Option<PathBuf>,
    replay: bool,
    elicit: bool,
) -> Result<Box<Opened>> {
    let loaded = {
        let cwd = cwd.clone();
        tokio::task::spawn_blocking(move || setup::load(&cwd))
            .await
            .map_err(|e| anyhow!("loading the config failed: {e}"))?
    };
    let Setup { cfgs, ctx, host, providers, notice_rx, .. } = setup::build(&cwd, loaded);
    let (spec, level) = pick_model(&cfgs, &providers, model.clone(), thinking)?;
    let run = agent::RunConfig {
        pin_model: model.is_some() && resume.is_some(),
        pin_thinking: thinking.is_some() && resume.is_some(),
        permission_mode: permissions,
        task_launch: crate::run::task_launch(None, &cfgs),
        // Plans are approved through `session/request_permission`, which every client has.
        excluded: if elicit { vec![] } else { vec!["ask"] },
        ..Default::default()
    };
    let AgentHandle { cmd: cmd_tx, ev: mut ev_rx, task, .. } = build_agent(
        AgentSetup {
            providers: providers.clone(),
            plugins: host.clone(),
            ctx: ctx.clone(),
            model: spec,
            thinking: level,
            deliver: crate::workflow::deliver::detached_starter(),
            run,
            output_schema: None,
        },
        cwd.clone(),
        Mode::Acp,
    );
    let mode_tools: Vec<String> = ctx.modes.iter().map(|m| m.exit.tool.clone()).collect();
    let known = resume.as_deref().map(list::id_of);
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
                    for u in translate_with(&cwd, e, false, &mode_tools) {
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
    let sid = known.unwrap_or_else(|| list::id_of(Path::new(st.session_path.as_deref().unwrap_or(""))));
    // Start from the cached list: no network wait before the session is usable.
    let mut models: Vec<String> = providers.cached_models().into_iter().map(|m| m.id).collect();
    if !models.contains(&st.model_id) {
        models.push(st.model_id.clone());
    }
    let session = Session {
        cmd: cmd_tx,
        cwd,
        ctx,
        host,
        task,
        prompt: None,
        failed: false,
        last_error: None,
        streamed: false,
        pending_cfg: None,
        models,
        advertised: Advertised { model: st.model_id, thinking: st.thinking, mode: st.mode },
        providers: providers.clone(),
        used: st.context_tokens,
        title: st.title,
        stream: Stream::default(),
        jobs: Jobs::default(),
        mode_tools,
        plan_call: None,
        epoch,
    };
    Ok(Box::new(Opened { sid, session, ev_rx, notice_rx, providers }))
}

/// Forwards a session's events. A closed channel without a prior `Exit` means the agent task died
/// (a panic drops its sender), so it ends the session as an exit would and answers the pending request.
async fn forward_events(
    sid: String,
    epoch: u64,
    rx: UnboundedReceiver<AgentEvent>,
    tx: UnboundedSender<(String, u64, AgentEvent)>,
) {
    forward(sid.clone(), epoch, rx, tx.clone(), |e| e).await;
    let _ = tx.send((sid, epoch, AgentEvent::Exit));
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

/// One `session/list` page out of `total` (already newest first and cut at `offset + PAGE + 1`), and the
/// cursor of the next page when more remain.
fn page(total: Vec<Value>, offset: usize) -> (Vec<Value>, Option<String>) {
    let more = total.len() > offset.saturating_add(PAGE);
    let items = total.into_iter().skip(offset).take(PAGE).collect();
    (items, more.then(|| offset.saturating_add(PAGE).to_string()))
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
    UserInput { text: parts.join("\n\n"), images, command: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dead_agent_task_ends_its_session() {
        let (etx, erx) = tokio::sync::mpsc::unbounded_channel();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let _etx = etx;
            panic!("agent died");
        });
        assert!(task.await.is_err());
        forward_events("s".into(), 3, erx, tx).await;
        let (sid, epoch, e) = rx.recv().await.expect("an event");
        assert_eq!((sid.as_str(), epoch), ("s", 3));
        assert!(matches!(e, AgentEvent::Exit));
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
        assert!(list::valid_id("0198abcd"));
        assert!(!list::valid_id(""));
        assert!(!list::valid_id("../x"));
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

    #[test]
    fn a_listing_pages_by_offset_and_names_the_next_page() {
        let all = |n: usize| (0..n).map(|i| json!({"sessionId": i})).collect::<Vec<_>>();
        assert_eq!(page(all(3), 0), (all(3), None));
        assert_eq!(page(all(100), 0), (all(100), None));
        let (items, next) = page(all(101), 0);
        assert_eq!((items.len(), next.as_deref()), (100, Some("100")));
        assert_eq!(page(all(101), 100), (vec![json!({"sessionId": 100})], None));
        assert_eq!(page(all(201), 100).1.as_deref(), Some("200"));
        assert!(page(all(3), 10).0.is_empty());
    }

    #[test]
    fn modes_are_default_and_the_command_modes() {
        assert_eq!((mode_id(None), mode_id(Some("plan"))), ("default", "plan"));
        let ctx = Context { modes: mu_tools::context::builtin_modes(), ..Default::default() };
        let ids: Vec<String> = modes(&ctx).into_iter().map(|m| m.0).collect();
        assert_eq!(ids, ["default", "plan"]);
        assert_eq!(modes(&ctx)[1].1, "Plan");
    }
}
