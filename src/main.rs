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
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn dispatch() -> Result<i32> {
    mu_provider::net::install_crypto();
    secrets::set_stored_refresh(mu_provider::auth::store::sync_redaction);
    let mut cli = Cli::parse();
    if let Some(p) = &cli.profile {
        // Single-threaded here: no runtime or plugin has started yet.
        unsafe { std::env::set_var("OMP_PROFILE", p) };
    }
    if let Some(t) = &cli.theme {
        unsafe { std::env::set_var("MU_THEME", t) };
    }
    match cli.command.take() {
        None => run::run(&cli, None),
        Some(Sub::Plan { cmd }) => match plans_cli::plan_run(cmd) {
            Ok(r) => run::run(&cli, Some(r)),
            Err(other) => Ok(plans_cli::run_cli(other, &std::env::current_dir()?)),
        },
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
        Sub::Web { cmd } => cli::web::run(cmd),
        Sub::Predict { cmd } => cli::predict::run(cmd),
        Sub::MigrateProfile { name, dry_run } => workflow::migrate::run(&name, dry_run),
        Sub::Plan { .. } => unreachable!("`mu plan` is routed in dispatch"),
    }
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
