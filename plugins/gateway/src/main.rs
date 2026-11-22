//! `mu-gateway`: one shared `omp auth-broker` + `omp auth-gateway` pair per omp profile, leased by client pid and
//! stopped when idle. Any client can use it; `mu-plugin` is the adapter for mu.

mod adapter;
mod commands;
mod daemon;
mod sealed_state;
mod state;

use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(name = "mu-gateway", version = env!("MU_GATEWAY_VERSION"), about)]
struct Cli {
    /// The omp profile to serve (default: $OMP_PROFILE).
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print {"baseUrl","token"} of the gateway, taking a lease for a client pid and starting the daemon if needed.
    Endpoint {
        /// The process the lease belongs to (default: this process's parent).
        #[arg(long)]
        client_pid: Option<u32>,
        /// Print null, and touch nothing, when no daemon is running.
        #[arg(long)]
        no_start: bool,
    },
    /// Print the parsed output of `omp models ls --json`.
    Prices,
    /// Print {"running","pid","url","leases"}.
    Status,
    /// Stop the daemon.
    Stop,
    /// Run `omp login [PROVIDER]`.
    Login { provider: Option<String> },
    /// Serve mu's gateway provider contract on stdio.
    MuPlugin,
    #[command(hide = true)]
    Daemon,
    #[command(hide = true)]
    Guard { pgid: i32 },
}

fn main() {
    let cli = Cli::parse();
    if let Some(p) = cli.profile.filter(|p| !p.is_empty()) {
        // SAFETY: still single-threaded; children (the daemon, omp) inherit the profile this way.
        unsafe { std::env::set_var("OMP_PROFILE", p) };
    }
    let out = match cli.cmd {
        Cmd::Daemon => std::process::exit(daemon::run()),
        Cmd::Guard { pgid } => std::process::exit(daemon::guard(pgid)),
        Cmd::MuPlugin => {
            adapter::run();
            return;
        }
        Cmd::Endpoint { client_pid, no_start } => {
            // SAFETY: getppid cannot fail.
            let pid = client_pid.unwrap_or_else(|| unsafe { libc::getppid() } as u32);
            commands::endpoint(pid, !no_start).map(|e| e.map_or(json!(null), |e| e.to_json()))
        }
        Cmd::Prices => commands::prices(),
        Cmd::Status => Ok(commands::status()),
        Cmd::Stop => Ok(json!({"stopped": commands::stop()})),
        Cmd::Login { provider } => Err(commands::login(provider.as_deref())),
    };
    match out {
        Ok(v) => println!("{v}"),
        Err(e) => {
            eprintln!("mu-gateway: {e}");
            std::process::exit(1);
        }
    }
}
