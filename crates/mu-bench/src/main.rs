//! `mu-bench`: runs mu in a terminal against a fake model server and measures resource use.
//! No real model is involved, so a run costs nothing and repeats.
mod driver;
mod report;
mod scenario;
mod server;
mod smaps;

use anyhow::{Result, bail};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "usage: mu-bench [--mu PATH] [--runs N] [--settle-ms MS] [--out FILE.json] SCENARIO.toml...

  --mu PATH        the mu binary to measure (default: target/release/mu)
  --runs N         repeats per scenario; the table shows medians (default: 3)
  --settle-ms MS   idle time before each memory sample (default: 2000)
  --out FILE       also write the full results as JSON";

struct Args {
    mu: PathBuf,
    runs: usize,
    settle: Duration,
    out: Option<PathBuf>,
    scenarios: Vec<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut a = Args {
        mu: "target/release/mu".into(),
        runs: 3,
        settle: Duration::from_millis(2000),
        out: None,
        scenarios: vec![],
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| anyhow::anyhow!("{name} needs a value\n\n{USAGE}"));
        match arg.as_str() {
            "--mu" => a.mu = value("--mu")?.into(),
            "--runs" => a.runs = value("--runs")?.parse()?,
            "--settle-ms" => a.settle = Duration::from_millis(value("--settle-ms")?.parse()?),
            "--out" => a.out = Some(value("--out")?.into()),
            "-h" | "--help" => bail!("{USAGE}"),
            s if s.starts_with("--") => bail!("unknown option {s}\n\n{USAGE}"),
            _ => a.scenarios.push(arg.into()),
        }
    }
    if a.scenarios.is_empty() {
        bail!("no scenario given\n\n{USAGE}");
    }
    if a.runs == 0 {
        bail!("--runs must be at least 1");
    }
    Ok(a)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e:#}");
        std::process::exit(2);
    }
}

async fn run() -> Result<()> {
    let a = parse_args(std::env::args().skip(1))?;
    let mu = std::fs::canonicalize(&a.mu).map_err(|e| anyhow::anyhow!("{}: {e}", a.mu.display()))?;
    let opts = driver::Opts { mu, settle: a.settle };
    let mut summaries = Vec::new();
    for path in &a.scenarios {
        let sc = Arc::new(scenario::Scenario::load(path)?);
        let mut runs = Vec::new();
        for i in 0..a.runs {
            eprintln!("{}: run {}/{}", sc.name, i + 1, a.runs);
            runs.push(driver::run_once(&sc, &opts).await?);
        }
        let s = report::summarize(&sc.name, &runs);
        report::print(&s);
        summaries.push((s, runs));
    }
    if let Some(out) = &a.out {
        let doc = serde_json::json!({
            "mu": opts.mu,
            "settle_ms": a.settle.as_millis() as u64,
            "scenarios": summaries.iter().map(|(s, runs)| serde_json::json!({"summary": s, "runs": runs})).collect::<Vec<_>>(),
        });
        std::fs::write(out, serde_json::to_string_pretty(&doc)?)?;
        eprintln!("wrote {}", out.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Args> {
        parse_args(a.iter().map(|s| s.to_string()))
    }

    #[test]
    fn options_and_scenarios() {
        let a = p(&["--runs", "5", "--settle-ms", "100", "--mu", "x/mu", "a.toml", "b.toml"]).unwrap();
        assert_eq!((a.runs, a.settle, a.mu.as_path()), (5, Duration::from_millis(100), std::path::Path::new("x/mu")));
        assert_eq!(a.scenarios.len(), 2);
    }

    #[test]
    fn bad_invocations_are_refused() {
        assert!(p(&[]).is_err());
        assert!(p(&["--runs", "0", "a.toml"]).is_err());
        assert!(p(&["--runs"]).is_err());
        assert!(p(&["--nope", "a.toml"]).is_err());
        assert!(p(&["--runs", "x", "a.toml"]).is_err());
    }
}
