mod agent;
mod compaction;
mod config;
mod context;
mod acp;
mod events;
mod gitinfo;
mod gateway;
mod headless;
mod image;
mod jobs;
mod mem;
mod message;
mod models;
mod pricing;
mod plugin;
mod quota;
mod provider;
mod session;
mod tools;
mod tui;

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
}

/// Everything a session needs that depends only on the working directory.
struct Setup {
    cfgs: Rc<config::Configs>,
    ctx: Rc<context::Context>,
    host: Rc<plugin::PluginHost>,
    providers: Rc<provider::Providers>,
    notice_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

fn setup(cwd: &std::path::Path) -> Setup {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let cfgs = Rc::new(config::Configs::load());
    let ctx = Rc::new(context::Context::load_all(cwd, &home, &paths::omp_agent_dir(), cfgs.omp.skills.enable_claude_user));
    let (notice_tx, notice_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (manifests, warnings) = plugin::manifest::discover(&paths::mu_home().join("plugins"));
    let host = plugin::PluginHost::new(
        manifests,
        warnings,
        notice_tx,
        plugin::InitParams { cwd: cwd.to_string_lossy().into(), ..Default::default() },
    );
    let providers = Rc::new(provider::Providers::new(cfgs.clone(), gateway::GatewaySource::new(host.clone())));
    host.set_providers(&providers);
    Setup { cfgs, ctx, host, providers, notice_rx }
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
    agent.agent_prompt = Some(format!("\nYou are running as the custom agent `{0}`. Follow its instructions exactly; they override your defaults.\n<agent name=\"{0}\">\n{1}\n</agent>\n", def.name, def.prompt.trim()));
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
    let cli_thinking = cli.thinking.as_deref().and_then(models::Thinking::parse);
    if cli.acp {
        return acp::run(cli.model, cli_thinking);
    }
    let cwd = std::env::current_dir()?;
    let Setup { cfgs, ctx, host, providers, notice_rx } = setup(&cwd);

    let mut task_payload: Option<serde_json::Value> = None;
    let mut sel = cli.model.clone();
    let mut agent_def: Option<context::AgentDef> = None;
    if let Some(name) = &cli.agent {
        match ctx.agents.iter().find(|a| &a.name == name) {
            Some(d) => agent_def = Some(d.clone()),
            None => bail!("unknown agent `{name}`; looked in .omp/agents and {}/agents", paths::omp_agent_dir().display()),
        }
    }
    if cli.headless {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        let p: serde_json::Value = serde_json::from_str(&input).map_err(|e| anyhow::anyhow!("bad subagent input: {e}"))?;
        let role = if p["agent"] == "scout" { "smol" } else { "task" };
        if let Some(name) = p["agent"].as_str().filter(|n| !matches!(*n, "task" | "scout")) {
            match ctx.agents.iter().find(|a| a.name == name) {
                Some(d) => agent_def = Some(d.clone()),
                None => bail!("unknown agent `{name}`"),
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
            Some(f) => Some(f),
            None => bail!("session {} not found", p.display()),
        },
        (_, true) => match session::latest_for(&cwd) {
            Some(p) => Some(p),
            None => bail!("no sessions for {} in {}", cwd.display(), paths::session_dir(&cwd).display()),
        },
        _ => None,
    };
    if cli.print.is_none() && !cli.headless && !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
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
    if cli.print.is_none() && task_payload.is_none() {
        agent.enable_jobs();
    }
    if let Some(p) = resume {
        let _ = cmd_tx.send(events::Command::Resume(p));
    }

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let local = tokio::task::LocalSet::new();
    let code = local.block_on(&rt, async move {
        let mut notice_rx = notice_rx;
        let agent_task = tokio::task::spawn_local(agent.run(cmd_rx));
        let (print, max_cost) = (cli.print, cli.max_cost);
        let code = match (print, task_payload) {
            (_, Some(p)) => {
                tokio::task::spawn_local(async move {
                    while let Some(n) = notice_rx.recv().await {
                        eprintln!("{n}");
                    }
                });
                let ctx = p["context"].as_str().filter(|c| !c.is_empty());
                let task = p["task"].as_str().unwrap_or("");
                let prompt = ctx.map_or(task.to_string(), |c| format!("{c}\n\n{task}"));
                headless::subagent_mode(prompt, cmd_tx, ev_rx).await
            }
            (Some(prompt), None) => {
                tokio::task::spawn_local(async move {
                    while let Some(n) = notice_rx.recv().await {
                        eprintln!("{n}");
                    }
                });
                headless::print_mode(prompt, cmd_tx, ev_rx, inbox.clone(), max_cost).await
            }
            (None, None) => {
                tui::theme::init(cfgs_ui.omp.theme.dark.as_deref().unwrap_or("dark"), &paths::omp_agent_dir());
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
                })
                .await
            }
        };
        let _ = agent_task.await;
        host.shutdown_all().await;
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
