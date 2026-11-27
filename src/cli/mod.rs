//! The `mu` command line: the flags and subcommands. Model selection lives in `select`.

pub mod agents;
pub mod attach;
pub mod auth;
mod chatgpt_login;
pub mod docs;
pub mod models;
pub mod predict;
pub mod providers;
mod secret;
pub mod select;
pub mod web;

use crate::workflow::{deliver, plans_cli};
use anyhow::{Result, bail};
use clap::Parser;
use std::path::PathBuf;

/// Passes when `--yes` was given or the user answered `y`/`yes` to the prompt.
pub(crate) fn approved(yes: bool, answer: Option<String>) -> Result<()> {
    if yes || answer.is_some_and(|a| matches!(a.trim().to_ascii_lowercase().as_str(), "y" | "yes")) {
        return Ok(());
    }
    bail!("approval needed: rerun with --yes after reading the text above")
}

#[derive(Parser)]
#[command(name = "mu", version = env!("MU_VERSION"))]
pub struct Cli {
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(mu_base::models::Thinking::NAMES))]
    pub thinking: Option<String>,
    /// Tool permissions: `ask`, `edits` or `yolo` (default: the config's mode).
    #[arg(long, global = true, value_parser = clap::builder::PossibleValuesParser::new(mu_base::permissions::PermissionMode::NAMES))]
    pub permissions: Option<String>,
    /// Resume the latest session for this directory.
    #[arg(short = 'c', long = "continue")]
    pub continue_latest: bool,
    /// Resume a session by id or path, or pick one when no value is given.
    #[arg(long, num_args = 0..=1)]
    pub resume: Option<Option<PathBuf>>,
    /// Config profile (sets MU_PROFILE; OMP_PROFILE and PI_PROFILE name one too).
    #[arg(long)]
    pub profile: Option<String>,
    /// Palette for this run, overriding `[theme] source` (a bundled name, `terminal`, or a file).
    #[arg(long)]
    pub theme: Option<String>,
    /// Run one prompt and print the final answer.
    #[arg(short = 'p', long = "print")]
    pub print: Option<String>,
    /// Run as a subagent: read one JSON task on stdin, write NDJSON results.
    #[arg(long, hide = true)]
    pub headless: bool,
    /// Run `-p` as a custom agent from `.omp/agents` or `<agent dir>/agents`.
    #[arg(long, requires = "print")]
    pub agent: Option<String>,
    /// Abort `-p` once the session has cost more than this many USD.
    #[arg(long, requires = "print")]
    pub max_cost: Option<f64>,
    /// Run the UI as a client of the `mu agent` that this shell command's stdin and stdout reach, for example
    /// `ssh host mu attach --stdio ID`. The command is run again when it drops. See docs/remote.md.
    #[arg(long, value_name = "COMMAND", conflicts_with_all = ["resume", "continue_latest", "print", "headless", "acp"])]
    pub attach: Option<String>,
    /// With `--attach`: the SSH ed25519 private key that unlocks a sealed agent (default ~/.ssh/id_ed25519).
    #[arg(long, value_name = "PATH", requires = "attach")]
    pub state_key: Option<PathBuf>,
    /// Serve the Agent Client Protocol over stdio.
    #[arg(long, conflicts_with_all = ["resume", "continue_latest", "print", "headless"])]
    pub acp: bool,
    #[command(subcommand)]
    pub command: Option<Sub>,
}

#[derive(clap::Subcommand)]
pub enum Sub {
    /// Check for a newer release and install it (uses the public GitHub API; falls back to an authenticated gh for a private fork).
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
    /// Provider API keys and the ChatGPT sign-in: `mu auth add|login|logout|list|rm|test`. A provider
    /// with a key or a sign-in is used directly, without omp.
    Auth {
        #[command(subcommand)]
        cmd: auth::Cmd,
    },
    /// Custom OpenAI-compatible endpoints (vLLM, Modal, ...): `mu providers list|add|models|rm`. Their models
    /// are selected as `<id>/<model id>`, without omp.
    Providers {
        #[command(subcommand)]
        cmd: providers::Cmd,
    },
    /// Where `web_search` sends queries: `mu web status|use|reset`. Nothing is contacted until you choose.
    Web {
        #[command(subcommand)]
        cmd: web::Cmd,
    },
    /// Model config: `mu models import` copies omp's or pi's model setup into mu's own files.
    Models {
        #[command(subcommand)]
        cmd: models::Cmd,
    },
    /// Ghost-text prediction. `mu predict setup smollm` downloads the opt-in SmolLM weights.
    Predict {
        #[command(subcommand)]
        cmd: predict::Cmd,
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
    /// Run the agent of a session with no terminal. It prints `{"session":ID,"socket":PATH}` and serves one
    /// client at a time on that Unix socket (see docs/remote.md); it keeps running when the client leaves.
    /// `mu agent list` lists the agents running here.
    Agent {
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(mu_base::models::Thinking::NAMES))]
        thinking: Option<String>,
        /// Resume this session (id or path) instead of starting a new one.
        #[arg(long)]
        resume: Option<PathBuf>,
        /// Resume the latest session for this directory.
        #[arg(short = 'c', long = "continue")]
        continue_latest: bool,
        /// Start the agent in the background: it leaves this terminal and session, logs to
        /// `<profile home>/logs/agent-<id>.log`, and this command prints `{"id":ID}` once it listens.
        #[arg(long, conflicts_with = "detached")]
        detach: bool,
        /// What `--detach` starts: an agent that ignores SIGHUP and has its output in a log.
        #[arg(long, hide = true)]
        detached: bool,
        #[command(subcommand)]
        cmd: Option<AgentCmd>,
    },
    /// Join a running `mu agent` from this process's stdin and stdout: copies lines to and from its socket.
    /// With no session it finds the one agent running in this directory.
    Attach {
        /// Required: the raw protocol stream, for `ssh host mu attach --stdio ID`.
        #[arg(long, required = true)]
        stdio: bool,
        /// Session id (prefix).
        session: Option<String>,
        /// A socket path, instead of a session.
        #[arg(long, conflicts_with = "session")]
        socket: Option<PathBuf>,
    },
    /// Print the version of each contract that programs reaching other machines use (docs/remote-contracts.md), as one JSON line.
    Contracts,
    /// Take over a running `mu -p` session: stop it after its current step, then resume it here.
    Take {
        /// Session file or id (prefix).
        session: PathBuf,
    },
    /// Durable plans: park a plan, then refine and approve it later (see `mu plan --help`).
    Plan {
        #[command(subcommand)]
        cmd: plans_cli::PlanCmd,
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
    /// Move sessions, plans, prompt history and other state that older mu kept in $MU_HOME for every
    /// profile into $MU_HOME/profiles/<NAME>, where that omp profile now finds it. Run it once, with
    /// no mu open, for the profile you used. Exit codes: 0 done, 1 something was left in place, 2 bad name.
    MigrateProfile {
        name: String,
        /// Print what would move without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Subcommand)]
pub enum AgentCmd {
    /// List the agents running here, one per line.
    List {
        /// One JSON object per line: id, cwd, started (unix seconds), title, client, waiting.
        #[arg(long)]
        json: bool,
    },
    /// End a running agent here: it ends its jobs and plugins and exits, as `/quit` does. ID is a session id or a prefix of one.
    Stop { id: String },
    /// Cancel the current turn of a running agent here; the agent keeps running.
    Cancel { id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_takes_no_session_or_print_flags() {
        assert!(Cli::try_parse_from(["mu", "--acp", "-c"]).is_err());
        assert!(Cli::try_parse_from(["mu", "--acp", "--resume", "abc"]).is_err());
        assert!(Cli::try_parse_from(["mu", "--acp", "-p", "x"]).is_err());
        assert!(Cli::try_parse_from(["mu", "--acp", "--model", "a/b"]).is_ok());
    }

    #[test]
    fn attach_takes_a_command_and_an_optional_state_key() {
        let c = Cli::try_parse_from(["mu", "--attach", "ssh h mu attach --stdio x", "--state-key", "/k"]).unwrap();
        assert_eq!(
            (c.attach.as_deref(), c.state_key.as_deref()),
            (Some("ssh h mu attach --stdio x"), Some("/k".as_ref()))
        );
        assert!(Cli::try_parse_from(["mu", "--state-key", "/k"]).is_err(), "a key needs --attach");
        assert!(Cli::try_parse_from(["mu", "--attach", "x", "-c"]).is_err());
        assert!(Cli::try_parse_from(["mu", "--attach", "x", "-p", "hi"]).is_err());
    }

    #[test]
    fn agent_and_attach_take_their_own_flags() {
        assert!(Cli::try_parse_from(["mu", "agent", "--model", "a/b", "--resume", "abc"]).is_ok());
        assert!(Cli::try_parse_from(["mu", "agent", "-c", "--permissions", "yolo"]).is_ok());
        assert!(Cli::try_parse_from(["mu", "attach"]).is_err(), "the raw pipe needs --stdio");
        assert!(Cli::try_parse_from(["mu", "attach", "--stdio"]).is_ok());
        assert!(Cli::try_parse_from(["mu", "attach", "--stdio", "abc"]).is_ok());
        assert!(Cli::try_parse_from(["mu", "attach", "--stdio", "abc", "--socket", "/x"]).is_err());
        assert!(Cli::try_parse_from(["mu", "agent", "--detach"]).is_ok());
        assert!(Cli::try_parse_from(["mu", "agent", "list", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["mu", "agent", "stop", "abc"]).is_ok());
        assert!(Cli::try_parse_from(["mu", "agent", "cancel", "abc"]).is_ok());
        assert!(Cli::try_parse_from(["mu", "agent", "stop"]).is_err());
    }
}
