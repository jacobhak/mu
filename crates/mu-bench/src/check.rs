//! `mu-bench check`: the memory gate. Runs a scenario as the default command does and compares each row's
//! incidental memory (private RAM the conversation content does not account for) with a committed baseline.
use crate::driver::{self, Opts};
use crate::report::{self, Summary};
use crate::scenario::Scenario;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const SETTLE: Duration = Duration::from_millis(2000);
/// Added to every limit so that rows of a few MB do not fail on measurement noise.
const SLACK_KB: f64 = 1024.0;

const USAGE: &str = "usage: mu-bench check [--mu PATH] [--runs N] [--baseline FILE] [--tolerance-pct P] [--update] [--out FILE.json] SCENARIO.toml
  Fails (exit 1) when a row's incidental memory is above baseline * (1 + P/100) + 1 MB.

  --mu PATH          the mu binary to measure (default: target/release/mu)
  --runs N           repeats; the medians are compared (default: 3)
  --baseline FILE    the committed baseline (default: crates/mu-bench/baselines/<scenario>.json)
  --tolerance-pct P  allowed growth over the baseline, in percent (default: 20)
  --update           write the measured values to the baseline and exit 0
  --out FILE         also write the full results as JSON";

#[derive(Debug, PartialEq)]
pub struct Args {
    pub mu: PathBuf,
    pub runs: usize,
    pub baseline: PathBuf,
    pub tolerance_pct: f64,
    pub update: bool,
    pub out: Option<PathBuf>,
    pub scenario: PathBuf,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub scenario: String,
    pub rows: Vec<BaselineRow>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct BaselineRow {
    pub label: String,
    pub incidental_kb: f64,
}

/// One row measured against its baseline.
#[derive(Debug, PartialEq)]
pub struct Verdict {
    pub label: String,
    pub measured_kb: f64,
    pub base_kb: f64,
    pub limit_kb: f64,
}

impl Verdict {
    pub fn ok(&self) -> bool {
        self.measured_kb <= self.limit_kb
    }
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut a = Args {
        mu: "target/release/mu".into(),
        runs: 3,
        baseline: PathBuf::new(),
        tolerance_pct: 20.0,
        update: false,
        out: None,
        scenario: PathBuf::new(),
    };
    let mut baseline = None;
    let mut scenarios = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| anyhow!("{name} needs a value\n\n{USAGE}"));
        match arg.as_str() {
            "--mu" => a.mu = value("--mu")?.into(),
            "--runs" => a.runs = value("--runs")?.parse()?,
            "--baseline" => baseline = Some(PathBuf::from(value("--baseline")?)),
            "--tolerance-pct" => a.tolerance_pct = value("--tolerance-pct")?.parse()?,
            "--update" => a.update = true,
            "--out" => a.out = Some(value("--out")?.into()),
            "-h" | "--help" => bail!("{USAGE}"),
            s if s.starts_with("--") => bail!("unknown option {s}\n\n{USAGE}"),
            _ => scenarios.push(PathBuf::from(arg)),
        }
    }
    let [scenario] = <[PathBuf; 1]>::try_from(scenarios).map_err(|_| anyhow!("give one scenario\n\n{USAGE}"))?;
    if a.runs == 0 {
        bail!("--runs must be at least 1");
    }
    if !(a.tolerance_pct.is_finite() && a.tolerance_pct >= 0.0) {
        bail!("--tolerance-pct must be a number of percent, 0 or more");
    }
    a.baseline = match baseline {
        Some(b) => b,
        None => {
            let stem = scenario.file_stem().context("scenario has no file name")?;
            PathBuf::from("crates/mu-bench/baselines").join(format!("{}.json", stem.to_string_lossy()))
        }
    };
    a.scenario = scenario;
    Ok(a)
}

/// Each row's incidental memory: private RAM (median) minus the content held (median).
pub fn incidental(s: &Summary) -> Vec<(String, f64)> {
    s.rows.iter().map(|r| (r.label.clone(), r.mu_anon_kb - r.content_kb)).collect()
}

pub fn limit_kb(base_kb: f64, tolerance_pct: f64) -> f64 {
    base_kb * (1.0 + tolerance_pct / 100.0) + SLACK_KB
}

/// Checks every measured row against the baseline. A row the baseline lacks is an error.
pub fn compare(measured: &[(String, f64)], baseline: &Baseline, tolerance_pct: f64) -> Result<Vec<Verdict>> {
    measured
        .iter()
        .map(|(label, kb)| {
            let Some(base) = baseline.rows.iter().find(|r| &r.label == label) else {
                bail!("baseline has no row {label}; run with --update");
            };
            Ok(Verdict {
                label: label.clone(),
                measured_kb: *kb,
                base_kb: base.incidental_kb,
                limit_kb: limit_kb(base.incidental_kb, tolerance_pct),
            })
        })
        .collect()
}

/// Returns whether every row is within its limit.
pub async fn main(args: impl IntoIterator<Item = String>) -> Result<bool> {
    let a = parse_args(args)?;
    let mu = std::fs::canonicalize(&a.mu).map_err(|e| anyhow!("{}: {e}", a.mu.display()))?;
    let baseline: Option<Baseline> = if a.update {
        None
    } else {
        let text = std::fs::read_to_string(&a.baseline)
            .with_context(|| format!("reading baseline {} (create it with --update)", a.baseline.display()))?;
        Some(serde_json::from_str(&text).with_context(|| format!("parsing baseline {}", a.baseline.display()))?)
    };
    let sc = Arc::new(Scenario::load(&a.scenario)?);
    let opts = Opts { mu: mu.clone(), settle: SETTLE };
    let mut runs = Vec::new();
    for i in 0..a.runs {
        eprintln!("{}: run {}/{}", sc.name, i + 1, a.runs);
        runs.push(driver::run_once(&sc, &opts).await?);
    }
    let s = report::summarize(&sc.name, sc.stream_rate(), &runs);
    report::print(&s);
    let measured = incidental(&s);
    if let Some(out) = &a.out {
        let doc = serde_json::json!({
            "mu": mu,
            "settle_ms": SETTLE.as_millis() as u64,
            "summary": s,
            "runs": runs,
            "incidental": measured.iter().map(|(l, kb)| serde_json::json!({"label": l, "incidental_kb": kb})).collect::<Vec<_>>(),
        });
        std::fs::write(out, serde_json::to_string_pretty(&doc)?)?;
        eprintln!("wrote {}", out.display());
    }
    let Some(baseline) = baseline else {
        let doc = Baseline {
            scenario: sc.name.clone(),
            rows: measured.iter().map(|(l, kb)| BaselineRow { label: l.clone(), incidental_kb: kb.round() }).collect(),
        };
        if let Some(dir) = a.baseline.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&a.baseline, serde_json::to_string_pretty(&doc)? + "\n")?;
        eprintln!("wrote baseline {}", a.baseline.display());
        return Ok(true);
    };
    let verdicts = compare(&measured, &baseline, a.tolerance_pct)?;
    println!("\nmemory check: incidental kB against {} (+{}% and 1 MB slack):", a.baseline.display(), a.tolerance_pct);
    for v in &verdicts {
        println!(
            "{:<8} measured {:>9.0}  baseline {:>9.0}  limit {:>9.0}  {}",
            v.label,
            v.measured_kb,
            v.base_kb,
            v.limit_kb,
            if v.ok() { "ok" } else { "OVER" }
        );
    }
    for v in verdicts.iter().filter(|v| !v.ok()) {
        eprintln!(
            "memory check failed: {} has {:.0} kB incidental memory, over its limit of {:.0} kB (baseline {:.0} kB)",
            v.label, v.measured_kb, v.limit_kb, v.base_kb
        );
    }
    Ok(verdicts.iter().all(Verdict::ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Args> {
        parse_args(a.iter().map(|s| s.to_string()))
    }

    fn baseline(rows: &[(&str, f64)]) -> Baseline {
        Baseline {
            scenario: "t".into(),
            rows: rows.iter().map(|(l, kb)| BaselineRow { label: l.to_string(), incidental_kb: *kb }).collect(),
        }
    }

    #[test]
    fn limit_is_percent_plus_one_mb() {
        assert_eq!(limit_kb(1000.0, 20.0), 2224.0);
        assert_eq!(limit_kb(0.0, 20.0), 1024.0);
    }

    #[test]
    fn compare_flags_only_rows_over_their_limit() {
        let base = baseline(&[("start", 1000.0), ("turn 1", 2000.0)]);
        let measured = [("start".to_string(), 2224.0), ("turn 1".to_string(), 3425.0)];
        let v = compare(&measured, &base, 20.0).unwrap();
        assert_eq!(v.iter().map(Verdict::ok).collect::<Vec<_>>(), [true, false]);
        assert_eq!(v[1].limit_kb, 3424.0);
    }

    #[test]
    fn compare_refuses_a_row_missing_from_the_baseline() {
        let base = baseline(&[("start", 1000.0)]);
        let err = compare(&[("turn 9".to_string(), 1.0)], &base, 20.0).unwrap_err();
        assert_eq!(err.to_string(), "baseline has no row turn 9; run with --update");
    }

    #[test]
    fn arguments_and_default_baseline() {
        let a = p(&["--runs", "2", "--update", "crates/mu-bench/scenarios/read-heavy.toml"]).unwrap();
        assert_eq!((a.runs, a.update, a.tolerance_pct), (2, true, 20.0));
        assert_eq!(a.baseline, PathBuf::from("crates/mu-bench/baselines/read-heavy.json"));
        let a = p(&["--baseline", "b.json", "--tolerance-pct", "5", "s.toml"]).unwrap();
        assert_eq!((a.baseline, a.tolerance_pct), (PathBuf::from("b.json"), 5.0));
        assert!(p(&[]).is_err());
        assert!(p(&["a.toml", "b.toml"]).is_err());
        assert!(p(&["--runs", "0", "a.toml"]).is_err());
        assert!(p(&["--tolerance-pct", "-1", "a.toml"]).is_err());
        assert!(p(&["--nope", "a.toml"]).is_err());
    }
}
