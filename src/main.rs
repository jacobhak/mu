mod cli;
mod run;
mod update;
mod workflow;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Sub};
use mu_base::secrets;
use workflow::{deliver, notify, plans_cli, review_cli, take};

fn main() {
    match dispatch() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    }
}

fn dispatch() -> Result<i32> {
    mu_common::paths::check_home().map_err(anyhow::Error::msg)?;
    mu_provider::net::install_crypto();
    secrets::set_stored_refresh(mu_provider::auth::store::sync_redaction).map_err(anyhow::Error::msg)?;
    cli::docs::register()?;
    let mut cli = Cli::parse();
    if let Some(p) = &cli.profile {
        // Single-threaded here: no runtime or plugin has started yet.
        unsafe { std::env::set_var("OMP_PROFILE", p) };
    }
    if let Some(t) = &cli.theme {
        unsafe { std::env::set_var("MU_THEME", t) };
    }
    if matches!(cli.command, None | Some(Sub::Plan { .. }) | Some(Sub::Agent { .. })) {
        std::thread::spawn(|| {
            mu_base::image::prune_blobs_now();
        });
    }
    match cli.command.take() {
        None => run::run(&cli, None),
        Some(Sub::Plan { cmd }) => match plans_cli::plan_run(cmd) {
            Ok(r) => run::run(&cli, Some(r)),
            Err(other) => Ok(plans_cli::run_cli(other, &std::env::current_dir()?)),
        },
        Some(Sub::Agent { cmd: Some(cli::AgentCmd::List { json }), .. }) => cli::agents::list(json),
        Some(Sub::Agent { cmd: Some(cli::AgentCmd::Stop { id }), .. }) => cli::agents::stop(&id),
        Some(Sub::Agent { cmd: Some(cli::AgentCmd::Cancel { id }), .. }) => cli::agents::cancel(&id),
        Some(Sub::Agent { detach: true, .. }) => run::detach::run(),
        Some(Sub::Agent { model, thinking, resume, continue_latest, detached, .. }) => {
            if detached {
                run::mark_detached();
            }
            cli.model = model.or(cli.model.take());
            cli.thinking = thinking.or(cli.thinking.take());
            cli.resume = resume.map(Some).or(cli.resume.take());
            cli.continue_latest |= continue_latest;
            run::run_agent(&cli)
        }
        Some(sub) => subcommand(sub),
    }
}

fn subcommand(sub: Sub) -> Result<i32> {
    match sub {
        Sub::Take { session } => take::run(&session, &std::env::current_dir()?),
        Sub::Notify { session, from, text } => notify::run(&session, from.as_deref(), &text, &std::env::current_dir()?),
        Sub::Review { base, plan, focus, out, markdown } => {
            Ok(review_cli::run_cli(review_cli::CliOpts { base, plan, focus, out, markdown }, &std::env::current_dir()?))
        }
        Sub::Deliver(a) => Ok(deliver::run_cli(a, &std::env::current_dir()?)),
        Sub::Update { check, force, tag } => update::run(check, force, tag),
        Sub::Auth { cmd } => cli::auth::run(cmd),
        Sub::Providers { cmd } => cli::providers::run(cmd),
        Sub::Web { cmd } => cli::web::run(cmd),
        Sub::Models { cmd } => cli::models::run(cmd),
        Sub::Predict { cmd } => cli::predict::run(cmd),
        Sub::MigrateProfile { name, dry_run } => workflow::migrate::run(&name, dry_run),
        Sub::Attach { session, socket, .. } => cli::attach::run(session.as_deref(), socket),
        Sub::Contracts => {
            println!("{}", mu_base::contracts::json());
            Ok(0)
        }
        Sub::Plan { .. } => unreachable!("`mu plan` is routed in dispatch"),
        Sub::Agent { .. } => unreachable!("`mu agent` is routed in dispatch"),
    }
}
