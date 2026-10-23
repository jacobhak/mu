mod agent;
mod anthropic;
mod config;
mod context;
mod mem;
mod message;
mod paths;
mod session;
mod supervise;
mod tools;
mod ui;

use anyhow::{Result, bail};
use clap::Parser;
use rustyline::{DefaultEditor, error::ReadlineError};
use std::path::PathBuf;

const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5-5";
const LEVELS: [&str; 4] = ["off", "low", "medium", "high"];

#[derive(Parser)]
#[command(name = "mu", version)]
struct Cli {
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(LEVELS))]
    thinking: Option<String>,
    /// Resume the latest session for this directory, or PATH.
    #[arg(long, num_args = 0..=1)]
    resume: Option<Option<PathBuf>>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let files = context::load(&cwd, &home, &paths::omp_agent_dir());
    let gateway = supervise::ensure_gateway()?;
    let gw = anthropic::Gateway::from_env(gateway.as_ref().map(|g| g.gateway_url.as_str()))?;

    let (cfg_model, cfg_level) = config::default_model().unzip();
    let mut model = cli.model.clone().or(cfg_model).unwrap_or_else(|| DEFAULT_MODEL.into());
    let mut thinking = cli.thinking.clone().or(cfg_level.flatten()).unwrap_or_else(|| "medium".into());
    let mut session = None;
    let mut messages = Vec::new();
    if let Some(r) = &cli.resume {
        let path = match r {
            Some(p) => p.clone(),
            None => match session::latest_for(&cwd) {
                Some(p) => p,
                None => bail!("no sessions for {} in {}", cwd.display(), paths::session_dir(&cwd).display()),
            },
        };
        let loaded = session::load(&path)?;
        let mut s = session::SessionFile::open_append(&path)?;
        match (&cli.model, loaded.model) {
            (Some(m), prev) => {
                if prev.as_deref() != Some(m) {
                    s.model_change(m);
                }
            }
            (None, Some(prev)) => model = prev,
            _ => {}
        }
        match (&cli.thinking, loaded.thinking) {
            (Some(t), prev) => {
                if prev.as_deref() != Some(t) {
                    s.thinking_change(t);
                }
            }
            (None, Some(prev)) => thinking = prev,
            _ => {}
        }
        messages = loaded.messages;
        session = Some(s);
    }

    println!(
        "mu · {model} · thinking {thinking} · {}",
        session.as_ref().map_or("new session".into(), |s| s.path.display().to_string())
    );

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(repl(agent::Agent::new(gw, cwd, files, model, thinking, session, messages)))
}

async fn repl(mut agent: agent::Agent) -> Result<()> {
    let mut rl = DefaultEditor::new()?;
    let mut interrupted = false;
    loop {
        let line = match rl.readline("› ") {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => {
                if interrupted {
                    break;
                }
                interrupted = true;
                agent.ui.line("(press Ctrl-C again or Ctrl-D to exit)");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        interrupted = false;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line);
        if let Some(cmd) = line.strip_prefix('/') {
            let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(n, a)| (n, a.trim()));
            match name {
                "exit" => break,
                "model" if !arg.is_empty() => agent.set_model(arg),
                "thinking" if LEVELS.contains(&arg) => agent.set_thinking(arg),
                "thinking" => agent.ui.error("usage: /thinking off|low|medium|high"),
                "session" => {
                    let p = agent.session.as_ref().map_or("no session yet".into(), |s| s.path.display().to_string());
                    agent.ui.line(&p)
                }
                _ => agent.ui.error("unknown command"),
            }
            continue;
        }
        agent.turn(line).await;
    }
    Ok(())
}
