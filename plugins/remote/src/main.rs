//! `mu-remote`: remotes for mu. Agents and tasks on other machines, reached through mu's documented entry
//! points (`mu --attach`, `[task] launcher`, `[task] launch`, `[sessions] sources`; see mu's
//! docs/remote.md) and an encrypted channel this program adds.

mod adapter;
mod client;
mod config;
mod conformance;
mod connect;
mod fs;
mod helper;
mod install;
mod launch;
mod link;
mod manage;
mod paths;
mod rows;
mod rpc;
mod secure;
mod ssh;
#[cfg(test)]
mod testutil;
mod trust;
mod worker;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mu-remote", version, args_conflicts_with_subcommands = true)]
struct Cli {
    /// The remote to use (`[remote.NAME]` in the config); default: `default_remote`.
    name: Option<String>,
    /// Join the agent that has this session (id), or start one that resumes it.
    #[arg(long, value_name = "ID", conflicts_with = "continue_latest")]
    resume: Option<PathBuf>,
    /// Join the latest agent, or start one that continues the latest session.
    #[arg(short = 'c', long = "continue")]
    continue_latest: bool,
    /// The model a newly started agent uses.
    #[arg(long)]
    model: Option<String>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// The stream to agent ID on remote NAME, for `mu --attach`: the adapter's `connect`, then the encrypted
    /// channel. Plain lines on stdin and stdout.
    Connect { name: String, id: String },
    /// On the remote: the far end of `connect`. Ends the encrypted channel and pipes to `mu attach --stdio ID`
    /// (or, for a worker's launch id, to the worker's socket).
    Bridge { id: String },
    /// Print the fingerprint of this machine's bridge key, to compare with the one `connect` pins.
    Key,
    /// A `[task] launcher`: runs the task of the environment (`MU_TASK_*`) as a worker on remote NAME.
    Launch { name: String },
    /// The worker's end on the remote (run by an adapter).
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// The configured remotes: adapter, whether trusted, whether it answers.
    List,
    /// Run an adapter's `version` and `list` and report what fails. With `--full` it also runs a synthetic worker
    /// through `start`, `connect`, `fetch` and `stop` and, with `--agent ID`, the file and port verbs.
    Check {
        name: String,
        #[arg(long)]
        full: bool,
        /// With `--full`: an agent on the remote to try the file and port verbs on (a trusted remote only).
        #[arg(long, requires = "full")]
        agent: Option<String>,
    },
    /// A `[sessions] sources` command: the agents on every remote (or NAME), as JSON lines.
    Rows { name: Option<String> },
    /// End agent (or worker) ID on remote NAME.
    Stop { name: String, id: String },
    /// Cancel the current turn of agent ID on remote NAME.
    Cancel { name: String, id: String },
    /// Leave a message for agent ID on remote NAME (`-` reads it from stdin).
    Notify {
        name: String,
        id: String,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// List the agents and workers on every remote, or on NAME.
    Agents {
        name: Option<String>,
        /// One JSON object per line.
        #[arg(long)]
        json: bool,
    },
    /// Write a shipped adapter into the state directory: `pod` is the Kubernetes one, `local` the reference
    /// adapter that runs everything on this machine.
    InstallAdapter { name: String },
    /// What mu's `MU_ATTACH_HELPER` runs: `forward`, `unforward`, `read` and `write` for agent ID on remote NAME.
    #[command(hide = true)]
    Helper {
        name: String,
        id: String,
        verb: String,
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum WorkerCmd {
    /// Serve worker `--launch ID` in `--dir`, a git checkout, until it is stopped.
    Run {
        #[arg(long)]
        launch: String,
        #[arg(long)]
        dir: PathBuf,
        /// Start the worker in the background and print `{"id":ID}` once it listens.
        #[arg(long, conflicts_with = "detached")]
        detach: bool,
        /// What `--detach` starts.
        #[arg(long, hide = true)]
        detached: bool,
    },
    /// Commit what the worker changed and print the result as a git bundle (nothing when it changed nothing).
    Bundle { id: String },
    /// End the worker, committing what it changed first.
    Stop { id: String },
}

fn main() {
    match dispatch() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("mu-remote: {e:#}");
            std::process::exit(1);
        }
    }
}

fn dispatch() -> Result<i32> {
    let cli = Cli::parse();
    match cli.command {
        None => client::run(cli.name, cli.resume, cli.continue_latest, cli.model),
        Some(Cmd::Connect { name, id }) => connect::connect(&name, &id),
        Some(Cmd::Bridge { id }) => connect::bridge(&id),
        Some(Cmd::Key) => connect::key(),
        Some(Cmd::Launch { name }) => launch::run(&name),
        Some(Cmd::Worker { cmd }) => match cmd {
            WorkerCmd::Run { launch, dir, detach, detached } => worker::run(&launch, &dir, detach, detached),
            WorkerCmd::Bundle { id } => worker::bundle(&id),
            WorkerCmd::Stop { id } => worker::stop(&id),
        },
        Some(Cmd::List) => rows::list(),
        Some(Cmd::Check { name, full, agent }) => rows::check(&name, full, agent.as_deref()),
        Some(Cmd::Rows { name }) => rows::rows(name.as_deref()),
        Some(Cmd::Stop { name, id }) => manage::stop(&name, &id),
        Some(Cmd::Cancel { name, id }) => manage::cancel(&name, &id),
        Some(Cmd::Notify { name, id, text }) => manage::notify(&name, &id, &text),
        Some(Cmd::Agents { name, json }) => manage::agents(name.as_deref(), json),
        Some(Cmd::InstallAdapter { name }) => install::run(&name),
        Some(Cmd::Helper { name, id, verb, args }) => helper::run(&name, &id, &verb, &args),
    }
}
