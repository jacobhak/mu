mod acp;
mod agent;
mod ask_nudge;
mod at_imports;
mod compaction;
mod config;
mod context;
mod cost;
mod deliver;
mod events;
mod gateway;
mod gitinfo;
mod headless;
mod image;
mod jobs;
mod jsoncache;
mod jtd;
mod mem;
mod message;
mod models;
mod notify;
mod plans;
mod plugin;
mod policy;
mod predict_setup;
mod pricing;
mod proc;
mod provider;
mod pty;
mod quota;
mod review;
mod schema;
mod session;
mod take;
#[cfg(test)]
mod testutil;
mod tools;
mod tui;
mod update;

use anyhow::{Result, bail};
use clap::Parser;
use mu_common::paths;
use std::path::PathBuf;
use std::rc::Rc;

const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5-5";

#[derive(Parser)]
#[command(name = "mu", version = env!("MU_VERSION"))]
struct Cli {
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(models::Thinking::NAMES))]
    thinking: Option<String>,
    /// Resume the latest session for this directory.
    #[arg(short = 'c', long = "continue")]
    continue_latest: bool,
    /// Resume a session by id or path, or pick one when no value is given.
    #[arg(long, num_args = 0..=1)]
    resume: Option<Option<PathBuf>>,
    /// Config profile (sets OMP_PROFILE).
    #[arg(long)]
    profile: Option<String>,
    /// Palette for this run, overriding `[theme] source` (a bundled name, `terminal`, or a file).
    #[arg(long)]
    theme: Option<String>,
    /// Run one prompt and print the final answer.
    #[arg(short = 'p', long = "print")]
    print: Option<String>,
    /// Run as a subagent: read one JSON task on stdin, write NDJSON results.
    #[arg(long, hide = true)]
    headless: bool,
    /// Run `-p` as a custom agent from `.omp/agents` or `<agent dir>/agents`.
    #[arg(long, requires = "print")]
    agent: Option<String>,
    /// Abort `-p` once the session has cost more than this many USD.
    #[arg(long, requires = "print")]
    max_cost: Option<f64>,
    /// Serve the Agent Client Protocol over stdio.
    #[arg(long)]
    acp: bool,
    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Check for a newer release and install it (needs an authenticated `gh`).
    Update {
        /// Only report whether a newer release exists.
        #[arg(long)]
        check: bool,
        /// Replace a dev build.
        #[arg(long)]
        force: bool,
        /// Install this release tag instead of the latest.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Ghost-text prediction. `mu predict setup smollm` downloads the opt-in SmolLM weights.
    Predict {
        #[command(subcommand)]
        cmd: predict_setup::Cmd,
    },
    /// Review the changes against a base the way `/review` does, without the TUI. Exit codes: 0 pass
    /// or nothing to review, 1 fixes needed, 2 the review could not complete.
    Review {
        /// Git ref to diff against; `HEAD` (the default) reviews uncommitted changes.
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// The approved plan the changes should match.
        #[arg(long)]
        plan: Option<PathBuf>,
        /// What the reviewer should look at hardest.
        #[arg(long)]
        focus: Option<String>,
        /// Write the output here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print markdown instead of JSON.
        #[arg(long)]
        markdown: bool,
    },
    /// Take over a running `mu -p` session: stop it after its current step, then resume it here.
    Take {
        /// Session file or id (prefix).
        session: PathBuf,
    },
    /// Durable plans: park a plan, then refine and approve it later (see `mu plan --help`).
    Plan {
        #[command(subcommand)]
        cmd: plans::PlanCmd,
    },
    /// Leave a message for a session; an interactive mu with it open wakes up and reads it.
    Notify {
        /// Session file or id (prefix).
        session: PathBuf,
        /// Who the message is from (shown to the model).
        #[arg(long)]
        from: Option<String>,
        /// The message; `-` reads it from stdin.
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Deliver a parked plan: approve and implement it, review it against the plan, send blocking findings back to the implementer (at most --max-fix rounds), then have it commit. Exit codes: 0 pass, 1 fixes still needed, 2 error or failed.
    Deliver(deliver::DeliverArgs),
}

/// Everything a session needs that depends only on the working directory.
struct Setup {
    cfgs: Rc<config::Configs>,
    ctx: Rc<context::Context>,
    host: Rc<plugin::PluginHost>,
    providers: Rc<provider::Providers>,
    notice_tx: tokio::sync::mpsc::UnboundedSender<String>,
    notice_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

/// Prints notices to stderr until told to stop, then prints whatever is still queued.
fn spawn_notice_printer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) -> (tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (done, mut done_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::task::spawn_local(async move {
        loop {
            tokio::select! {
                n = rx.recv() => match n {
                    Some(n) => eprintln!("{n}"),
                    None => break,
                },
                _ = &mut done_rx => {
                    while let Ok(n) = rx.try_recv() {
                        eprintln!("{n}");
                    }
                    break;
                }
            }
        }
    });
    (done, handle)
}

fn setup(cwd: &std::path::Path) -> Setup {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let cfgs = Rc::new(config::Configs::load());
    let ctx =
        Rc::new(context::Context::load_all(cwd, &home, &paths::omp_agent_dir(), cfgs.omp.skills.enable_claude_user));
    let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Err(e) = tools::edit::set_mode(&cfgs.mu.edit.mode) {
        let _ = notice_tx.send(format!("warning: {e}"));
    }
    for w in cfgs.warnings.iter().chain(&ctx.warnings) {
        let _ = notice_tx.send(format!("warning: {w}"));
    }
    let (manifests, mut warnings) = plugin::manifest::discover(&paths::mu_home().join("plugins"));
    let (servers, mcp_warnings) = plugin::mcp::config::load(cwd, &paths::omp_agent_dir());
    warnings.extend(mcp_warnings);
    let mcp_lifetime = match plugin::manifest::Lifetime::parse(&cfgs.mu.mcp.lifetime) {
        Some(l) => l,
        None => {
            if !cfgs.mu.mcp.lifetime.is_empty() {
                warnings.push(format!("[mcp] lifetime `{}` is not valid; using session", cfgs.mu.mcp.lifetime));
            }
            plugin::manifest::Lifetime::Session
        }
    };
    let host = plugin::PluginHost::new_with_mcp(
        manifests,
        servers,
        mcp_lifetime,
        plugin::mcp::cache::dir(),
        warnings,
        notice_tx.clone(),
        plugin::InitParams { cwd: cwd.to_string_lossy().into(), ..Default::default() },
    );
    let providers = Rc::new(provider::Providers::new(cfgs.clone(), gateway::GatewaySource::new(host.clone())));
    host.set_providers(&providers);
    Setup { cfgs, ctx, host, providers, notice_tx, notice_rx }
}

/// Model and thinking level for a new agent: `sel` (command line or headless role) beats the `default` role.
fn pick_model(
    cfgs: &config::Configs,
    providers: &provider::Providers,
    sel: Option<String>,
    cli_thinking: Option<models::Thinking>,
) -> Result<(models::ModelSpec, models::Thinking)> {
    let (cfg_model, cfg_level) = cfgs.role("default").map(|v| models::split_level(&v)).unzip();
    let sel = sel.or(cfg_model).unwrap_or_else(|| DEFAULT_MODEL.into());
    let (spec, role_level) = providers.resolve(&sel)?;
    Ok((spec, cli_thinking.or(role_level).or(cfg_level.flatten()).unwrap_or(models::Thinking::Medium)))
}

/// First `provider/model[:level]` or configured `@role` in the agent's model list, else the `task` role chain.
fn agent_selector(def: &context::AgentDef, cfgs: &config::Configs) -> Option<String> {
    def.model
        .iter()
        .find(|m| match m.strip_prefix('@') {
            Some(role) => cfgs.role(models::split_level(role).0.as_str()).is_some(),
            None => m.contains('/'),
        })
        .cloned()
        .or_else(|| models::role_selector("task", cfgs))
}

fn apply_agent(agent: &mut agent::Agent, def: &context::AgentDef) {
    agent.allow = def.tools.clone();
    agent.excluded = vec!["ask", "propose_plan"];
    agent.agent_prompt = Some(format!(
        "\nYou are running as the custom agent `{0}`. Follow its instructions exactly; they override your defaults.\n<agent name=\"{0}\">\n{1}\n</agent>\n",
        def.name,
        def.prompt.trim()
    ));
}

/// The result schema of a `--headless` run: the caller's `outputSchema`, else the agent file's `output:` (as omp).
fn task_output_schema(payload: &serde_json::Value, def: Option<&context::AgentDef>) -> Option<serde_json::Value> {
    payload.get("outputSchema").filter(|s| !s.is_null()).cloned().or_else(|| def.and_then(|d| d.output.clone()))
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    if let Some(p) = &cli.profile {
        // Single-threaded here: no runtime or plugin has started yet.
        unsafe { std::env::set_var("OMP_PROFILE", p) };
    }
    if let Some(t) = &cli.theme {
        unsafe { std::env::set_var("MU_THEME", t) };
    }
    if let Some(Sub::Take { session }) = cli.command {
        return take::run(&session, &std::env::current_dir()?);
    }
    if let Some(Sub::Notify { session, from, text }) = cli.command {
        return notify::run(&session, from.as_deref(), &text, &std::env::current_dir()?);
    }
    if let Some(Sub::Review { base, plan, focus, out, markdown }) = cli.command {
        return Ok(review::run_cli(review::CliOpts { base, plan, focus, out, markdown }, &std::env::current_dir()?));
    }
    if let Some(Sub::Deliver(a)) = cli.command {
        return Ok(deliver::run_cli(a, &std::env::current_dir()?));
    }
    if let Some(Sub::Update { check, force, tag }) = cli.command {
        return update::run(check, force, tag);
    }
    if let Some(Sub::Predict { cmd }) = cli.command {
        return predict_setup::run(cmd);
    }
    let plan_run = match cli.command {
        Some(Sub::Plan { cmd }) => match plans::plan_run(cmd) {
            Ok(r) => Some(r),
            Err(other) => return Ok(plans::run_cli(other, &std::env::current_dir()?)),
        },
        _ => None,
    };
    let headless_run = cli.print.is_some() || plan_run.is_some();
    let cli_thinking = cli.thinking.as_deref().and_then(models::Thinking::parse);
    if cli.acp {
        return acp::run(cli.model, cli_thinking);
    }
    let cwd = std::env::current_dir()?;
    let Setup { cfgs, mut ctx, host, providers, notice_tx, notice_rx } = setup(&cwd);
    if !headless_run && !cli.headless && cfgs.mu.update.check {
        // Background release check at most once a day; silent when offline or on a dev build.
        let tx = notice_tx.clone();
        std::thread::spawn(move || {
            if let Some(n) = update::startup_notice() {
                let _ = tx.send(n);
            }
        });
    }
    drop(notice_tx);

    let mut task_payload: Option<serde_json::Value> = None;
    let mut sel = cli.model.clone();
    let mut agent_def: Option<context::AgentDef> = None;
    if let Some(name) = &cli.agent {
        match ctx.agents.iter().find(|a| &a.name == name) {
            Some(d) => agent_def = Some(d.clone()),
            None => {
                bail!("unknown agent `{name}`; looked in .omp/agents and {}/agents", paths::omp_agent_dir().display())
            }
        }
    }
    if cli.headless {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        let p: serde_json::Value =
            serde_json::from_str(&input).map_err(|e| anyhow::anyhow!("bad subagent input: {e}"))?;
        let role = if p["agent"] == "scout" { "smol" } else { "task" };
        if let Some(name) = p["agent"].as_str().filter(|n| !matches!(*n, "task" | "scout")) {
            match ctx.agents.iter().find(|a| a.name == name).cloned().or_else(|| context::builtin_agent(name)) {
                Some(d) => agent_def = Some(d),
                None => bail!("unknown agent `{name}`"),
            }
            if name == context::REVIEW_LEAD {
                // Only the lead's `task` schema lists the worker; its children resolve it by the same fallback.
                let w = context::builtin_agent(context::REVIEW_WORKER).expect("builtin");
                Rc::get_mut(&mut ctx).expect("context not shared yet").agents.push(w);
            }
        }
        if cli.model.is_none() {
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
    let (spec, thinking) = pick_model(&cfgs, &providers, sel, cli_thinking.or(def_thinking))?;

    let pick_session = matches!(cli.resume, Some(None)) && !cli.continue_latest;
    let resume = match (&cli.resume, cli.continue_latest) {
        (Some(Some(p)), _) => match session::find(&cwd, p) {
            Ok(f) => Some(f),
            Err(e) => bail!("{e}"),
        },
        (_, true) => match session::latest_for(&cwd) {
            Some(p) => Some(p),
            None => bail!("no sessions for {} in {}", cwd.display(), paths::session_dir(&cwd).display()),
        },
        _ => None,
    };
    if !headless_run && !cli.headless && !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        bail!("mu: interactive mode needs a terminal; use -p");
    }

    let cfgs_ui = cfgs.clone();
    let (providers_ui, ctx_ui, cwd_ui) = (providers.clone(), ctx.clone(), cwd.clone());
    let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let inbox = events::new_inbox();
    let mut agent = agent::Agent::new(agent::AgentInit {
        providers,
        plugins: host.clone(),
        cwd,
        ctx: ctx.clone(),
        model: spec,
        thinking,
        events: ev_tx,
        inbox: inbox.clone(),
    });
    agent.exit_on_resume_failure = cli.print.is_some();
    agent.pin_model = cli.model.is_some() && resume.is_some();
    agent.pin_thinking = cli_thinking.is_some() && resume.is_some();
    if let Some(p) = &task_payload {
        agent.subagent = true;
        agent.excluded = vec!["ask", "propose_plan"];
        agent.parent_session = p["parentSession"].as_str().map(PathBuf::from);
        if p["agent"] == "scout" {
            agent.allow = Some(["read", "grep", "glob", "web_search", "fetch"].map(String::from).to_vec());
        }
    }
    if let Some(d) = &agent_def {
        apply_agent(&mut agent, d);
    }
    let output_schema = task_payload.as_ref().and_then(|p| task_output_schema(p, agent_def.as_ref()));
    if let Some(s) = &output_schema {
        if let Err(e) = schema::check_schema(s) {
            bail!("bad subagent input: {e}");
        }
        *agent.tools.output_schema.borrow_mut() = Some(s.clone());
        let note = "\nThis task has an output schema. Finish by calling the `yield` tool with `data` matching it; a plain final message is not a result.\n";
        agent.agent_prompt = Some(agent.agent_prompt.take().unwrap_or_default() + note);
    }
    if headless_run && !agent.excluded.contains(&"ask") {
        // No one can answer in `-p` mode.
        agent.excluded.push("ask");
    }
    if let Some(n) = plan_run.as_ref().and_then(|r| r.note) {
        agent.agent_prompt = Some(agent.agent_prompt.take().unwrap_or_default() + n);
    }
    let mut notify_wake = None;
    if !headless_run && task_payload.is_none() {
        agent.enable_jobs();
        notify_wake = agent.enable_notify();
    }
    if let Some(p) = resume {
        let _ = cmd_tx.send(events::Command::Resume(p));
    }

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&rt, async move {
        let notice_rx = notice_rx;
        // Before the agent task can create or lock a session: an owner record that says
        // `print_run` promises SIGUSR1 stops the run instead of killing it.
        let (stop_tx, stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        if headless_run && task_payload.is_none() {
            let mut sig = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("mu: cannot install the SIGUSR1 handler: {e}");
                    return 1;
                }
            };
            session::set_print_run();
            tokio::task::spawn_local(async move {
                while sig.recv().await.is_some() {
                    if stop_tx.send(()).is_err() {
                        break;
                    }
                }
            });
        }
        // Same order as above: the handler first, then the record flag, then the agent task.
        if let Some(wake) = notify_wake {
            match notify::forward_signal(wake) {
                Ok(()) => session::set_inbox(),
                Err(e) => {
                    eprintln!("mu: cannot install the SIGUSR2 handler ({e}); `mu notify` messages will only queue")
                }
            }
        }
        let agent_task = tokio::task::spawn_local(agent.run(cmd_rx));
        let (print, max_cost) = (cli.print, cli.max_cost);
        let mut printer = None;
        let code = match (print, task_payload, plan_run) {
            (_, Some(p), _) => {
                printer = Some(spawn_notice_printer(notice_rx));
                plugin::exit_on_signals();
                let ctx = p["context"].as_str().filter(|c| !c.is_empty());
                let task = p["task"].as_str().unwrap_or("");
                let prompt = ctx.map_or(task.to_string(), |c| format!("{c}\n\n{task}"));
                headless::subagent_mode(prompt, output_schema, cmd_tx, ev_rx).await
            }
            (Some(prompt), None, _) => {
                printer = Some(spawn_notice_printer(notice_rx));
                plugin::exit_on_signals();
                host.warnings.iter().for_each(|w| eprintln!("warning: {w}"));
                let first = events::Command::Prompt(prompt.into());
                headless::print_mode(first, false, cmd_tx, ev_rx, inbox.clone(), max_cost, stop_rx).await
            }
            (None, None, Some(r)) => {
                printer = Some(spawn_notice_printer(notice_rx));
                plugin::exit_on_signals();
                host.warnings.iter().for_each(|w| eprintln!("warning: {w}"));
                headless::print_mode(r.first, r.expect_park, cmd_tx, ev_rx, inbox.clone(), None, stop_rx).await
            }
            (None, None, None) => {
                host.set_interactive();
                tui::app::run(tui::app::AppInit {
                    cmd_tx,
                    ev_rx,
                    inbox,
                    providers: providers_ui,
                    ctx: ctx_ui,
                    plugins: host.clone(),
                    notice_rx,
                    cwd: cwd_ui,
                    pick_session,
                    status_cfg: cfgs_ui.omp.status_line.clone(),
                    theme: cfgs_ui.mu.theme.clone(),
                    notify: cfgs_ui.omp.completion.notify,
                })
                .await
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

#[cfg(test)]
mod contract_tests {
    #[test]
    #[ignore]
    fn contract_omp_version() {
        let out = mu_common::paths::run_omp_blocking(&["--version"], &[]);
        assert_eq!(out.trim(), format!("omp/{}", env!("MU_OMP_VERSION")));
    }
}

#[cfg(test)]
mod output_schema_tests {
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
