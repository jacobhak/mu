//! The front ends. `run` decides the `Mode` once from the command line, sets up one agent, and hands
//! it to that mode's function.

mod acp;
pub mod attach;
pub mod detach;
mod headless;
mod relay;
mod serve;
pub mod setup;
mod signals;
pub mod worker;

use crate::cli::Cli;
use crate::cli::select::{agent_selector, model_label, pick_model, pick_model_or_none, pick_model_with};
use crate::workflow::{deliver, plans_cli::PlanRun};
use anyhow::{Result, anyhow, bail};
use mu_agent::agent;
use mu_agent::handle::{AgentSetup, build_agent};
use mu_base::permissions::PermissionMode;
use mu_base::{events, models, schema};
use mu_common::paths;
use mu_session::session::{inbox, list, owner};
use mu_tools::{context, plugin};
use serde_json::{Value, json};
use setup::{Setup, spawn_notice_printer};
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// What this process does, with the data only that mode has. Decided once, from the command line.
enum Mode {
    /// The interactive TUI.
    Tui { pick_session: bool },
    /// `-p PROMPT`, or `mu plan new|refine|approve`: a headless run that starts from one command.
    Print(PrintRun),
    /// `--headless`: one subagent task read from stdin, NDJSON on stdout.
    Subagent { task: Value },
    /// `mu agent`: the agent with no terminal, serving a client over a Unix socket.
    Agent { resumed: bool },
}

struct PrintRun {
    first: events::Command,
    expect_park: bool,
    max_cost: Option<f64>,
}

/// A mode whose signal handlers are installed.
enum Armed {
    Tui { pick_session: bool },
    Print { run: PrintRun, stop: UnboundedReceiver<()> },
    Subagent { task: Value },
    Agent { resumed: bool },
}

impl Mode {
    /// `task` is the subagent task, read exactly when `--headless` is set; `agent` is `mu agent`.
    fn of(cli: &Cli, plan: Option<PlanRun>, task: Option<Value>, agent: bool, resumed: bool) -> Self {
        if agent {
            Self::Agent { resumed }
        } else if let Some(task) = task {
            Self::Subagent { task }
        } else if let Some(prompt) = &cli.print {
            Self::Print(PrintRun {
                first: events::Command::Prompt(prompt.clone().into()),
                expect_park: false,
                max_cost: cli.max_cost,
            })
        } else if let Some(r) = plan {
            Self::Print(PrintRun { first: r.first, expect_park: r.expect_park, max_cost: None })
        } else {
            Self::Tui { pick_session: matches!(cli.resume, Some(None)) && !cli.continue_latest }
        }
    }

    /// Whether `of` gives the TUI, before the task is read.
    fn interactive(cli: &Cli, plan: bool) -> bool {
        !cli.headless && cli.print.is_none() && !plan
    }

    /// The agent's own mode: only the TUI and `mu agent` keep background jobs and `mu notify` delivery.
    fn agent_mode(&self) -> mu_agent::handle::Mode {
        match self {
            Self::Tui { .. } | Self::Agent { .. } => mu_agent::handle::Mode::Tui,
            _ => mu_agent::handle::Mode::Headless,
        }
    }

    /// The kind the session's owner record names.
    fn run_kind(&self) -> owner::RunKind {
        match self {
            Self::Tui { .. } => owner::RunKind::Tui,
            Self::Agent { .. } => owner::RunKind::Agent,
            Self::Subagent { .. } => owner::RunKind::Subagent,
            Self::Print(_) => owner::RunKind::Print,
        }
    }

    /// Installs the handlers the mode promises in the session's owner record. Before the agent task
    /// can create or lock a session: an owner record that says `print_run` promises SIGUSR1 stops
    /// the run instead of killing it.
    fn arm(self) -> std::io::Result<Armed> {
        Ok(match self {
            Self::Tui { pick_session } => Armed::Tui { pick_session },
            Self::Print(run) => Armed::Print { run, stop: signals::print_run_stop()? },
            Self::Subagent { task } => Armed::Subagent { task },
            Self::Agent { resumed } => Armed::Agent { resumed },
        })
    }
}

/// Runs the front end `cli` asks for. `plan` is the first step of a `mu plan` command, if that is what ran.
pub fn run(cli: &Cli, plan: Option<PlanRun>) -> Result<i32> {
    run_as(cli, plan, false)
}

/// The agent was started by `mu agent --detach`: it ignores SIGHUP and moves its stdout to its log.
pub fn mark_detached() {
    serve::DETACHED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// `mu agent`: the agent of a session, with no terminal.
pub fn run_agent(cli: &Cli) -> Result<i32> {
    run_as(cli, None, true)
}

fn run_as(cli: &Cli, plan: Option<PlanRun>, agent: bool) -> Result<i32> {
    let cli_thinking = cli.thinking.as_deref().and_then(models::Thinking::parse);
    let permissions = cli.permissions.as_deref().and_then(PermissionMode::parse);
    if cli.acp {
        return acp::run(cli.model.clone(), cli_thinking, permissions);
    }
    if let Some(command) = &cli.nested_client {
        return attach::run(attach::Link::Command(command.clone()), false);
    }
    if Mode::interactive(cli, plan.is_some()) && !agent {
        let cfgs = mu_base::config::Configs::load();
        match crate::remote::client::in_effect(cli.remote.as_deref(), cli.local, &cfgs)? {
            Some(adapter) => {
                return crate::remote::client::run(adapter, cli.resume.clone(), cli.continue_latest, cli.model.clone());
            }
            None => {
                if let Some(rows) = crate::remote::client::tab_rows(&cfgs) {
                    mu_tui::host::sessions::set_remote_rows(rows);
                }
            }
        }
    }
    // `terminal`: the TUI. `interactive`: someone can answer a question or an approval, here or
    // through a client (the TUI and `mu agent`).
    let terminal = Mode::interactive(cli, plan.is_some()) && !agent;
    let interactive = terminal || agent;
    if matches!(cli.resume, Some(None)) && !terminal {
        bail!(
            "--resume without a session opens the session picker, which needs the interactive mode; pass a session id or path"
        );
    }
    let mut cwd = std::env::current_dir()?;
    let mut resume = find_resume(cli, &cwd)?;
    // A mirror of a launched worker's session opens in a worktree at what the worker produced.
    if let Some(p) = &mut resume
        && let Some(dir) = mu_session::session::mirror::checkout(p).map_err(|e| anyhow!("{e}"))?
    {
        if let Ok(abs) = std::fs::canonicalize(p.as_path()) {
            *p = abs;
        }
        std::env::set_current_dir(&dir)?;
        cwd = std::env::current_dir()?;
    }
    let Setup { cfgs, ctx, host, providers, notice_tx, notice_rx } = setup::setup(&cwd);
    if terminal && cfgs.mu.update.check {
        // Background release check at most once a day; silent when offline or on a dev build.
        let tx = notice_tx.clone();
        std::thread::spawn(move || {
            if let Some(n) = crate::update::startup_notice() {
                let _ = tx.send(n);
            }
        });
    }
    drop(notice_tx);

    let mut agent_def = named_agent(cli, &ctx)?;
    let task = if cli.headless {
        let (payload, def) = read_task(&ctx)?;
        if def.is_some() {
            agent_def = def;
        }
        Some(payload)
    } else {
        None
    };
    let sel = select_model(cli, &cfgs, agent_def.as_ref(), task.as_ref())?;
    let def_thinking = agent_def.as_ref().and_then(|d| d.thinking.as_deref()).and_then(models::Thinking::parse);
    let role_sel = sel.clone();
    let level = cli_thinking.or(def_thinking);
    // A relayed worker has no credentials: its parent resolves the selector, before any runtime exists.
    let stdio = task.as_ref().filter(|p| relays(p)).map(|_| Rc::new(relay::StdioRelay::new()));
    let (spec, thinking) = if let Some(r) = &stdio {
        let picked = pick_model_with(&cfgs, sel, level, |s| {
            let resolved = relay::resolve_first(s)?;
            providers.remember_relayed(s, &resolved.spec, resolved.thinking);
            Ok((resolved.spec, resolved.thinking))
        })?;
        providers.set_relay(r.clone());
        picked
    } else if interactive {
        pick_model_or_none(&cfgs, &providers, sel, level)?
    } else {
        pick_model(&cfgs, &providers, sel, level)?
    };
    if stdio.is_some() {
        host.set_relay_tools(task.as_ref().and_then(|p| p.get("relayTools")));
        host.set_relay_allow(task.as_ref().and_then(|p| p.get("relayModels")));
    }
    // The one reader of stdin, from here on: relay answers go to the relay, the rest to the subagent run.
    let lines = if task.is_some() {
        relay::spawn_reader(stdio.as_ref().map(|r| r.routes()))
    } else {
        tokio::sync::mpsc::unbounded_channel().1
    };
    let model_label = model_label(role_sel.as_deref(), &spec.id);

    if terminal && !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        bail!("mu: interactive mode needs a terminal; use -p");
    }
    if let (true, Some(p)) = (interactive, &resume) {
        // As `mu take`, asked first when there is a terminal to ask on: a `-p` run that has the session
        // is stopped after its current step.
        if let Some(code) = crate::workflow::take::release(p, "mu", terminal)? {
            return Ok(code);
        }
    }

    let mut run = agent::RunConfig {
        exit_on_resume_failure: cli.print.is_some(),
        pin_model: cli.model.is_some() && resume.is_some(),
        pin_thinking: cli_thinking.is_some() && resume.is_some(),
        permission_mode: permissions,
        ..Default::default()
    };
    if let Some(p) = &task {
        apply_task(&mut run, p, &cfgs);
    }
    run.task_launch = task_launch(task.as_ref(), &cfgs, cli.local);
    if let Some(d) = &agent_def {
        apply_agent(&mut run, d);
    }
    let output_schema = task.as_ref().and_then(|p| task_output_schema(p, agent_def.as_ref()));
    if let Some(s) = &output_schema {
        require_output_schema(&mut run, s)?;
    }
    if !interactive {
        // No one can answer or approve in a headless run.
        for tool in ["ask", "enter_plan"] {
            if !run.excluded.contains(&tool) {
                run.excluded.push(tool);
            }
        }
    }
    if let Some(n) = plan.as_ref().and_then(|r| r.note) {
        run.agent_prompt = Some(run.agent_prompt.take().unwrap_or_default() + n);
    }
    let name = task.as_ref().and_then(|p| p["agent"].as_str()).or(agent_def.as_ref().map(|d| d.name.as_str()));
    let name = name.map(String::from);
    let mode = Mode::of(cli, plan, task, agent, resume.is_some());
    owner::set_run_info(mode.run_kind(), name.as_deref());

    let cfgs_ui = cfgs.clone();
    let (providers_ui, ctx_ui, cwd_ui) = (providers.clone(), ctx.clone(), cwd.clone());
    let agent_mode = mode.agent_mode();
    let rt = mu_base::runtime::current_thread_runtime()?;
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&rt, async move {
        let armed = match mode.arm() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("mu: cannot install the SIGUSR1 handler: {e}");
                return 1;
            }
        };
        let mut handle = build_agent(
            AgentSetup {
                providers,
                plugins: host.clone(),
                ctx: ctx.clone(),
                model: spec,
                thinking,
                deliver: deliver::detached_starter(),
                run,
                output_schema: output_schema.clone(),
            },
            cwd,
            agent_mode,
        );
        if let Some(p) = resume {
            let _ = handle.cmd.send(events::Command::Resume(p));
        }
        // Same order as above: the handler first, then the record flag, then the agent task.
        if let Some(wake) = handle.wake.take() {
            match inbox::forward_signal(wake) {
                Ok(()) => owner::set_inbox(),
                Err(e) => {
                    eprintln!("mu: cannot install the SIGUSR2 handler ({e}); `mu notify` messages will only queue")
                }
            }
        }
        let agent_task = handle.task;
        let (code, printer) = match armed {
            Armed::Tui { pick_session } => {
                signals::cancel_turn_on_sigusr1(handle.cmd.clone());
                let ui = Ui { cfgs: cfgs_ui, providers: providers_ui, ctx: ctx_ui, cwd: cwd_ui, pick_session };
                let (cmd, ev) = if mu_base::wire::loopback_on() {
                    mu_base::wire::loopback(handle.cmd, handle.ev)
                } else {
                    (handle.cmd, handle.ev)
                };
                (tui(&host, ui, cmd, ev, notice_rx).await, None)
            }
            Armed::Agent { resumed } => {
                signals::cancel_turn_on_sigusr1(handle.cmd.clone());
                host.set_interactive();
                let local = mu_tui::host::local::LocalHost::new(providers_ui, ctx_ui, host.clone(), cwd_ui);
                let init = serve::Init {
                    host: Rc::new(local),
                    cmd: handle.cmd,
                    ev: handle.ev,
                    notices: notice_rx,
                    resumed,
                    config: cfgs_ui.mu.agent.clone(),
                };
                (serve::run(init).await, None)
            }
            Armed::Subagent { task } => {
                let printer = spawn_notice_printer(notice_rx);
                plugin::process::exit_on_signals();
                (subagent(&task, model_label, output_schema, handle.cmd, handle.ev, lines).await, Some(printer))
            }
            Armed::Print { run, stop } => {
                let printer = spawn_notice_printer(notice_rx);
                plugin::process::exit_on_signals();
                host.warnings.iter().for_each(|w| eprintln!("warning: {w}"));
                (print(run, stop, handle.cmd, handle.ev, host.relay_spend()).await, Some(printer))
            }
        };
        let code = match agent_task.await {
            Err(e) => {
                eprintln!("mu: agent task failed: {e}");
                1
            }
            Ok(()) => code,
        };
        host.shutdown_all().await;
        // Notices raised during shutdown are printed before the process exits.
        if let Some((done, handle)) = printer {
            let _ = done.send(());
            let _ = handle.await;
        }
        code
    });
    Ok(code)
}

/// The agent `--agent` names, from the project's or the user's agent files.
fn named_agent(cli: &Cli, ctx: &context::Context) -> Result<Option<context::AgentDef>> {
    let Some(name) = &cli.agent else { return Ok(None) };
    match ctx.agents.iter().find(|a| &a.name == name) {
        Some(d) => Ok(Some(d.clone())),
        None => bail!("unknown agent `{name}`; looked in .omp/agents and {}/agents", paths::omp_agent_dir().display()),
    }
}

/// The model selector: `--model`, else the subagent task's own `model` or its role, else the agent file's.
fn select_model(
    cli: &Cli,
    cfgs: &mu_base::config::Configs,
    agent_def: Option<&context::AgentDef>,
    task: Option<&Value>,
) -> Result<Option<String>> {
    if cli.model.is_some() {
        return Ok(cli.model.clone());
    }
    let Some(p) = task else { return Ok(agent_def.and_then(|d| agent_selector(d, cfgs))) };
    if let Some(m) = p["model"].as_str() {
        let role = m.trim_start_matches('@');
        return Ok(Some(
            models::role_selector(role, cfgs).ok_or_else(|| anyhow!(mu_base::roles::not_configured(role, cfgs)))?,
        ));
    }
    let role = if p["agent"] == context::SCOUT { context::SCOUT } else { context::TASK };
    Ok(match agent_def {
        Some(d) => agent_selector(d, cfgs),
        None => models::role_selector(role, cfgs),
    })
}

/// The session `--resume` or `--continue` names.
fn find_resume(cli: &Cli, cwd: &std::path::Path) -> Result<Option<std::path::PathBuf>> {
    Ok(match (&cli.resume, cli.continue_latest) {
        (Some(Some(p)), _) => match list::find(cwd, p) {
            Ok(f) => Some(f),
            Err(e) => bail!("{e}"),
        },
        (_, true) => match list::latest_for(cwd) {
            Some(p) => Some(p),
            None => bail!("no sessions for {} in {}", cwd.display(), paths::session_dir(cwd).display()),
        },
        _ => None,
    })
}

/// What a subagent run takes from its task: its tools, its parent and its request budget.
fn apply_task(run: &mut agent::RunConfig, p: &Value, cfgs: &mu_base::config::Configs) {
    run.subagent = true;
    run.excluded = agent::HEADLESS_EXCLUDED.to_vec();
    run.parent_session = p["parentSession"].as_str().map(std::path::PathBuf::from);
    run.inherited_permissions = p.get("permissions").cloned();
    // A budget the parent cannot raise: `[task] soft_request_budget`, capped per agent.
    let agent_name = p["agent"].as_str().unwrap_or(context::TASK);
    run.request_budget = mu_tools::tools::task::soft_request_budget(agent_name, cfgs.mu.task.soft_request_budget);
    run.budget_notice = cfgs.mu.task.budget_notice;
    run.mirror_session = mirrors_session(p);
}

/// Whether the parent asked this run to send its model calls to it.
fn relays(p: &Value) -> bool {
    p["relay"] == true
}

/// Whether the parent asked for this run's session file over the stream.
fn mirrors_session(p: &Value) -> bool {
    p["mirrorSession"] == true
}

/// The remote `task` subagents of some kinds run on. A worker (it has a task payload) gets none: it
/// holds a copy of its parent's config and spawns its own children locally.
fn task_launch(
    task: Option<&Value>,
    cfgs: &mu_base::config::Configs,
    local: bool,
) -> Option<mu_tools::tools::link::WorkerLaunch> {
    if task.is_some() {
        return None;
    }
    crate::remote::worker::launch(&cfgs.mu, local).unwrap_or_else(|e| {
        eprintln!("warning: {e}");
        None
    })
}

fn require_output_schema(run: &mut agent::RunConfig, s: &Value) -> Result<()> {
    if let Err(e) = schema::check_schema(s) {
        bail!("bad subagent input: {e}");
    }
    let note = "\nThis task has an output schema. Finish by calling the `yield` tool with `data` matching it; a plain final message is not a result.\n";
    run.agent_prompt = Some(run.agent_prompt.take().unwrap_or_default() + note);
    Ok(())
}

/// What the TUI needs besides the agent's channels.
struct Ui {
    cfgs: Rc<mu_base::config::Configs>,
    providers: Rc<mu_provider::provider::client::Providers>,
    ctx: Rc<context::Context>,
    cwd: std::path::PathBuf,
    pick_session: bool,
}

/// The interactive TUI until the user quits.
async fn tui(
    host: &Rc<plugin::host::PluginHost>,
    ui: Ui,
    cmd_tx: UnboundedSender<events::Command>,
    ev_rx: UnboundedReceiver<events::AgentEvent>,
    notice_rx: UnboundedReceiver<String>,
) -> i32 {
    host.set_interactive();
    let local = mu_tui::host::local::LocalHost::new(ui.providers.clone(), ui.ctx.clone(), host.clone(), ui.cwd.clone());
    // Loopback: every host call answers later, as a remote host's do.
    let host_for_ui: Rc<dyn mu_tui::host::Host> =
        if mu_base::wire::loopback_on() { Rc::new(mu_tui::host::Deferred(local)) } else { Rc::new(local) };
    mu_tui::app::run(mu_tui::app::AppInit {
        host: host_for_ui,
        cmd_tx,
        ev_rx,
        notice_rx,
        cwd: ui.cwd,
        pick_session: ui.pick_session,
        status_cfg: ui.cfgs.omp.status_line.clone(),
        theme: ui.cfgs.mu.theme.clone(),
        notify: ui.cfgs.omp.completion.notify,
        bell: ui.cfgs.mu.bell,
        hide_thinking: ui.cfgs.omp.hide_thinking_block,
        smooth_streaming: ui.cfgs.omp.display.smooth_streaming,
        status_levels: ui.cfgs.mu.status,
    })
    .await
}

/// `-p` and `mu plan`: the first command runs to the end of its turn, the answer on stdout.
async fn print(
    run: PrintRun,
    stop: UnboundedReceiver<()>,
    cmd: UnboundedSender<events::Command>,
    ev: UnboundedReceiver<events::AgentEvent>,
    spend: Rc<mu_tools::tools::relay::RelaySpend>,
) -> i32 {
    headless::print_mode_with(run.first, run.expect_park, cmd, ev, run.max_cost, stop, Some(spend)).await
}

/// `--headless`: the task's prompt runs as a subagent, its `model` line first.
async fn subagent(
    p: &Value,
    model_label: String,
    output_schema: Option<Value>,
    cmd: UnboundedSender<events::Command>,
    ev: UnboundedReceiver<events::AgentEvent>,
    lines: UnboundedReceiver<String>,
) -> i32 {
    let extra = p["context"].as_str().filter(|c| !c.is_empty());
    let task = p["task"].as_str().unwrap_or("");
    let prompt = extra.map_or(task.to_string(), |c| format!("{c}\n\n{task}"));
    println!("{}", json!({"type": "model", "label": model_label}));
    headless::subagent_mode(prompt, output_schema, mirrors_session(p), cmd, ev, lines).await
}

/// The subagent task on stdin, and the agent it names: a project agent, else one of mu's built-in
/// `task` and `scout`, whose tool allowlist and instructions then come from that definition.
fn read_task(ctx: &context::Context) -> Result<(Value, Option<context::AgentDef>)> {
    // Only the first line: later lines are the parent's approval answers, read as they arrive.
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    task_line(ctx, &input)
}

/// A worker runs only the version of mu that sent it the task: the wire between them is not
/// versioned otherwise. A payload that carries no version (a hand-written one) is accepted.
fn check_version(p: &Value) -> Result<()> {
    match p["muVersion"].as_str() {
        Some(v) if v != mu_tools::tools::subagent::MU_VERSION => bail!(
            "this worker is mu {} but the parent that sent the task is mu {v}; use the same version on both",
            mu_tools::tools::subagent::MU_VERSION
        ),
        _ => Ok(()),
    }
}

/// The first line as a payload and the agent it names; a payload without `agent` runs the worker.
fn task_line(ctx: &context::Context, input: &str) -> Result<(Value, Option<context::AgentDef>)> {
    let p: Value = serde_json::from_str(input).map_err(|e| anyhow::anyhow!("bad subagent input: {e}"))?;
    check_version(&p)?;
    let mut def = None;
    if let Some(name) = p["agent"].as_str() {
        match ctx.agents.iter().find(|a| a.name == name).cloned().or_else(|| context::builtin_agent(name)) {
            Some(d) => def = Some(d),
            None => bail!("unknown agent `{name}`"),
        }
    }
    Ok((p, def))
}

fn apply_agent(run: &mut agent::RunConfig, def: &context::AgentDef) {
    run.allow = def.tools.clone();
    run.excluded = agent::HEADLESS_EXCLUDED.to_vec();
    run.agent_prompt = Some(format!(
        "\nYou are running as the agent `{0}`. Follow its instructions exactly; they override your defaults.\n<agent name=\"{0}\">\n{1}\n</agent>\n",
        def.name,
        def.prompt.trim()
    ));
}

/// The result schema of a `--headless` run: the caller's `outputSchema`, else the agent file's `output:` (as omp).
fn task_output_schema(payload: &Value, def: Option<&context::AgentDef>) -> Option<Value> {
    payload.get("outputSchema").filter(|s| !s.is_null()).cloned().or_else(|| def.and_then(|d| d.output.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_process_started_with_a_task_payload_spawns_locally() {
        let mut cfgs = mu_base::config::Configs::default();
        cfgs.mu.task.launch = vec!["task".into()];
        cfgs.mu.task.remote = Some("far".into());
        cfgs.mu
            .remote
            .insert("far".into(), mu_base::config::MuRemote { adapter: "/bin/adapter".into(), ..Default::default() });
        assert!(task_launch(Some(&json!({"task": "t"})), &cfgs, false).is_none());
        assert!(task_launch(None, &cfgs, false).is_some_and(|l| l.runs("task") && !l.runs("scout")));
    }

    #[test]
    fn task_output_schema_prefers_caller_then_agent() {
        let def = context::AgentDef {
            name: "x".into(),
            description: "d".into(),
            tools: None,
            model: vec![],
            thinking: None,
            prompt: String::new(),
            output: Some(json!({"type":"object"})),
        };
        assert_eq!(
            task_output_schema(&json!({"outputSchema":{"type":"string"}}), Some(&def)),
            Some(json!({"type":"string"}))
        );
        assert_eq!(task_output_schema(&json!({"outputSchema":null}), Some(&def)), Some(json!({"type":"object"})));
        assert_eq!(task_output_schema(&json!({}), None), None);
    }

    /// A `scout` payload carries no agent file, so the built-in one has to supply both the
    /// instructions and the read-only allowlist; the worker gets every tool the run offers.
    #[test]
    fn a_scout_payload_gets_the_builtin_scout() {
        let ctx = context::Context::default();
        let (_, def) = task_line(&ctx, "{\"task\":\"t\",\"agent\":\"scout\"}\n").unwrap();
        let scout = def.expect("a scout");
        assert_eq!(scout.name, context::SCOUT);
        assert_eq!(
            scout.tools.as_deref(),
            Some(&["read".to_string(), "grep".into(), "glob".into(), "web_search".into(), "fetch".into()][..])
        );
        assert!(scout.prompt.contains("no shell") && !scout.prompt.is_empty());
        let (_, def) = task_line(&ctx, "{\"task\":\"t\",\"agent\":\"task\"}\n").unwrap();
        assert!(def.expect("a worker").tools.is_none(), "a worker gets every tool the run offers");
        let (_, def) = task_line(&ctx, "{\"task\":\"t\"}\n").unwrap();
        assert!(def.is_none(), "no agent in the payload means the default worker");
        assert!(task_line(&ctx, "{\"task\":\"t\",\"agent\":\"nope\"}\n").is_err());
    }
}
