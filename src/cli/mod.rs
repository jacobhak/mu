//! The `mu` command line: the flags and subcommands. Model selection lives in `select`.

pub mod auth;
pub mod models;
pub mod predict;
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
    /// Resume the latest session for this directory.
    #[arg(short = 'c', long = "continue")]
    pub continue_latest: bool,
    /// Resume a session by id or path, or pick one when no value is given.
    #[arg(long, num_args = 0..=1)]
    pub resume: Option<Option<PathBuf>>,
    /// Config profile (sets OMP_PROFILE).
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
    /// Serve the Agent Client Protocol over stdio.
    #[arg(long)]
    pub acp: bool,
    #[command(subcommand)]
    pub command: Option<Sub>,
}

#[derive(clap::Subcommand)]
pub enum Sub {
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
    /// Provider API keys and the ChatGPT sign-in: `mu auth add|login|logout|list|rm|test`. A provider
    /// with a key or a sign-in is used directly, without omp.
    Auth {
        #[command(subcommand)]
        cmd: auth::Cmd,
    },
    /// Where `web_search` sends queries: `mu web status|use|reset`. Nothing is contacted until you choose.
    Web {
        #[command(subcommand)]
        cmd: web::Cmd,
    },
    /// Model config: `mu models import` copies omp's model roles, presets and limits into models.toml.
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
