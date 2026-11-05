//! The front ends. `run` decides the `Mode` once from the command line, sets up one agent, and hands
//! it to that mode's function.

mod acp;
mod headless;
pub mod setup;
mod signals;

use crate::cli::Cli;
use crate::cli::select::{agent_selector, model_label, pick_model};
use crate::workflow::{deliver, plans_cli::PlanRun};
use anyhow::{Result, bail};
use mu_agent::agent;
use mu_agent::handle::{AgentSetup, build_agent};
use mu_base::{events, models, schema};
use mu_common::paths;
use mu_session::session::{inbox, list, owner};
use mu_tools::{context, plugin};
use serde_json::{Value, json};
use setup::{Setup, spawn_notice_printer};
use std::rc::Rc;

/// What this process does. Decided once, from the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The interactive TUI.
    Tui,
    /// `-p PROMPT`: one prompt, the final answer on stdout.
    Print,
    /// `mu plan new|refine|approve`: a headless run that starts from a plan command.
    Plan,
    /// `--headless`: one subagent task read from stdin, NDJSON on stdout.
    Subagent,
}

impl Mode {
    fn of(cli: &Cli, plan: bool) -> Self {
        if cli.headless {
            Self::Subagent
        } else if cli.print.is_some() {
            Self::Print
        } else if plan {
            Self::Plan
        } else {
            Self::Tui
        }
    }

    /// The agent's own mode: only the TUI keeps background jobs and `mu notify` delivery.
    fn agent_mode(self) -> mu_agent::handle::Mode {
        match self {
            Self::Tui => mu_agent::handle::Mode::Tui,
            _ => mu_agent::handle::Mode::Headless,
        }
    }

    /// The kind the session's owner record names.
    fn run_kind(self) -> owner::RunKind {
        match self {
            Self::Tui => owner::RunKind::Tui,
            Self::Subagent => owner::RunKind::Subagent,
            Self::Print | Self::Plan => owner::RunKind::Print,
        }
    }
}

/// Runs the front end `cli` asks for. `plan` is the first step of a `mu plan` command, if that is what ran.
pub fn run(cli: &Cli, plan: Option<PlanRun>) -> Result<i32> {
    let cli_thinking = cli.thinking.as_deref().and_then(models::Thinking::parse);
    if cli.acp {
        return acp::run(cli.model.clone(), cli_thinking);
    }
    let mode = Mode::of(cli, plan.is_some());
    let cwd = std::env::current_dir()?;
    let Setup { cfgs, mut ctx, host, providers, notice_tx, notice_rx } = setup::setup(&cwd);
    if mode == Mode::Tui && cfgs.mu.update.check {
        // Background release check at most once a day; silent when offline or on a dev build.
        let tx = notice_tx.clone();
        std::thread::spawn(move || {
            if let Some(n) = crate::update::startup_notice() {
                let _ = tx.send(n);
            }
        });
    }
    drop(notice_tx);

    let mut agent_def: Option<context::AgentDef> = None;
    if let Some(name) = &cli.agent {
        match ctx.agents.iter().find(|a| &a.name == name) {
            Some(d) => agent_def = Some(d.clone()),
            None => {
                bail!("unknown agent `{name}`; looked in .omp/agents and {}/agents", paths::omp_agent_dir().display())
            }
        }
    }
    let mut sel = cli.model.clone();
    let mut task_payload: Option<Value> = None;
    if mode == Mode::Subagent {
        let (p, def) = read_task(&mut ctx)?;
        if def.is_some() {
            agent_def = def;
        }
        if cli.model.is_none() {
            let role = if p["agent"] == "scout" { "smol" } else { "task" };
            sel = match &agent_def {
                Some(d) => agent_selector(d, &cfgs),
                None => models::role_selector(role, &cfgs),
            };
        }
        task_payload = Some(p);
    } else if cli.model.is_none() {
        sel = agent_def.as_ref().and_then(|d| agent_selector(d, &cfgs));
    }
    let def_thinking = agent_def.as_ref().and_then(|d| d.thinking.as_deref()).and_then(models::Thinking::parse);
    let role_sel = sel.clone();
    let (spec, thinking) = pick_model(&cfgs, &providers, sel, cli_thinking.or(def_thinking))?;
    let model_label = model_label(role_sel.as_deref(), &spec.id);

    let pick_session = matches!(cli.resume, Some(None)) && !cli.continue_latest;
    let resume = match (&cli.resume, cli.continue_latest) {
        (Some(Some(p)), _) => match list::find(&cwd, p) {
            Ok(f) => Some(f),
            Err(e) => bail!("{e}"),
        },
        (_, true) => match list::latest_for(&cwd) {
            Some(p) => Some(p),
            None => bail!("no sessions for {} in {}", cwd.display(), paths::session_dir(&cwd).display()),
        },
        _ => None,
    };
    if mode == Mode::Tui && !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        bail!("mu: interactive mode needs a terminal; use -p");
    }
    if let (Mode::Tui, Some(p)) = (mode, &resume) {
        // As `mu take`: a `-p` run that has the session is stopped after its current step, then it is ours.
        if let Some(code) = crate::workflow::take::release(p, "mu")? {
            return Ok(code);
        }
    }

    let cfgs_ui = cfgs.clone();
    let (providers_ui, ctx_ui, cwd_ui) = (providers.clone(), ctx.clone(), cwd.clone());
    let mut run = agent::RunConfig {
        exit_on_resume_failure: cli.print.is_some(),
        pin_model: cli.model.is_some() && resume.is_some(),
        pin_thinking: cli_thinking.is_some() && resume.is_some(),
        ..Default::default()
    };
    if let Some(p) = &task_payload {
        run.subagent = true;
        run.excluded = agent::HEADLESS_EXCLUDED.to_vec();
        run.parent_session = p["parentSession"].as_str().map(std::path::PathBuf::from);
        if p["agent"] == "scout" {
            run.allow = Some(["read", "grep", "glob", "web_search", "fetch"].map(String::from).to_vec());
        }
    }
    if let Some(d) = &agent_def {
        apply_agent(&mut run, d);
    }
    let output_schema = task_payload.as_ref().and_then(|p| task_output_schema(p, agent_def.as_ref()));
    if let Some(s) = &output_schema {
        if let Err(e) = schema::check_schema(s) {
            bail!("bad subagent input: {e}");
        }
        let note = "\nThis task has an output schema. Finish by calling the `yield` tool with `data` matching it; a plain final message is not a result.\n";
        run.agent_prompt = Some(run.agent_prompt.take().unwrap_or_default() + note);
    }
    if mode != Mode::Tui && !run.excluded.contains(&"ask") {
        // No one can answer in a headless run.
        run.excluded.push("ask");
    }
    if let Some(n) = plan.as_ref().and_then(|r| r.note) {
        run.agent_prompt = Some(run.agent_prompt.take().unwrap_or_default() + n);
    }
    let name = task_payload.as_ref().and_then(|p| p["agent"].as_str()).or(agent_def.as_ref().map(|d| d.name.as_str()));
    owner::set_run_info(mode.run_kind(), name);

    let (print, max_cost) = (cli.print.clone(), cli.max_cost);
    let rt = mu_base::runtime::current_thread_runtime()?;
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&rt, async move {
        // Before the agent task can create or lock a session: an owner record that says
        // `print_run` promises SIGUSR1 stops the run instead of killing it.
        let stop_rx = match mode {
            Mode::Print | Mode::Plan => match signals::print_run_stop() {
                Ok(rx) => Some(rx),
                Err(e) => {
                    eprintln!("mu: cannot install the SIGUSR1 handler: {e}");
                    return 1;
                }
            },
            Mode::Tui | Mode::Subagent => None,
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
            mode.agent_mode(),
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
        if mode == Mode::Tui {
            signals::cancel_turn_on_sigusr1(handle.cmd.clone());
        }
        let agent_task = handle.task;
        let (code, printer) = match mode {
            Mode::Tui => (
                tui(&host, cfgs_ui, providers_ui, ctx_ui, cwd_ui, pick_session, handle.cmd, handle.ev, notice_rx).await,
                None,
            ),
            Mode::Subagent => {
                let printer = spawn_notice_printer(notice_rx);
                plugin::process::exit_on_signals();
                let p = task_payload.expect("a subagent run has its task");
                (subagent(&p, model_label, output_schema, handle.cmd, handle.ev).await, Some(printer))
            }
            Mode::Print | Mode::Plan => {
                let printer = spawn_notice_printer(notice_rx);
                plugin::process::exit_on_signals();
                host.warnings.iter().for_each(|w| eprintln!("warning: {w}"));
                let (first, expect_park, max_cost) = match (print, plan) {
                    (Some(prompt), _) => (events::Command::Prompt(prompt.into()), false, max_cost),
                    (None, Some(r)) => (r.first, r.expect_park, None),
                    (None, None) => unreachable!("a print or plan run has a prompt or a plan"),
                };
                let stop = stop_rx.expect("a print or plan run stops on SIGUSR1");
                let code = headless::print_mode(first, expect_park, handle.cmd, handle.ev, max_cost, stop).await;
                (code, Some(printer))
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

/// The interactive TUI until the user quits.
#[allow(clippy::too_many_arguments)]
async fn tui(
    host: &Rc<plugin::host::PluginHost>,
    cfgs: Rc<mu_base::config::Configs>,
    providers: Rc<mu_provider::provider::client::Providers>,
    ctx: Rc<context::Context>,
    cwd: std::path::PathBuf,
    pick_session: bool,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<events::Command>,
    ev_rx: tokio::sync::mpsc::UnboundedReceiver<events::AgentEvent>,
    notice_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) -> i32 {
    host.set_interactive();
    mu_tui::app::run(mu_tui::app::AppInit {
        cmd_tx,
        ev_rx,
        providers,
        ctx,
        plugins: host.clone(),
        notice_rx,
        cwd,
        pick_session,
        status_cfg: cfgs.omp.status_line.clone(),
        theme: cfgs.mu.theme.clone(),
        notify: cfgs.omp.completion.notify,
    })
    .await
}

/// `--headless`: the task's prompt runs as a subagent, its `model` line first.
async fn subagent(
    p: &Value,
    model_label: String,
    output_schema: Option<Value>,
    cmd: tokio::sync::mpsc::UnboundedSender<events::Command>,
    ev: tokio::sync::mpsc::UnboundedReceiver<events::AgentEvent>,
) -> i32 {
    let extra = p["context"].as_str().filter(|c| !c.is_empty());
    let task = p["task"].as_str().unwrap_or("");
    let prompt = extra.map_or(task.to_string(), |c| format!("{c}\n\n{task}"));
    println!("{}", json!({"type": "model", "label": model_label}));
    headless::subagent_mode(prompt, output_schema, cmd, ev).await
}

/// The subagent task on stdin, and the agent it names (`task` and `scout` are the built-in roles).
fn read_task(ctx: &mut Rc<context::Context>) -> Result<(Value, Option<context::AgentDef>)> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    let p: Value = serde_json::from_str(&input).map_err(|e| anyhow::anyhow!("bad subagent input: {e}"))?;
    let mut def = None;
    if let Some(name) = p["agent"].as_str().filter(|n| !matches!(*n, "task" | "scout")) {
        match ctx.agents.iter().find(|a| a.name == name).cloned().or_else(|| context::builtin_agent(name)) {
            Some(d) => def = Some(d),
            None => bail!("unknown agent `{name}`"),
        }
        if name == context::REVIEW_LEAD {
            // Only the lead's `task` schema lists the worker; its children resolve it by the same fallback.
            let w = context::builtin_agent(context::REVIEW_WORKER).expect("builtin");
            Rc::get_mut(ctx).expect("context not shared yet").agents.push(w);
        }
    }
    Ok((p, def))
}

fn apply_agent(run: &mut agent::RunConfig, def: &context::AgentDef) {
    run.allow = def.tools.clone();
    run.excluded = agent::HEADLESS_EXCLUDED.to_vec();
    run.agent_prompt = Some(format!(
        "\nYou are running as the custom agent `{0}`. Follow its instructions exactly; they override your defaults.\n<agent name=\"{0}\">\n{1}\n</agent>\n",
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
}
