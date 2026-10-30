//! `mu-bench compare`: the same one-prompt scenario run headless by each harness against the fake model
//! server. Reports wall time and memory summed over the harness's whole process tree.
use crate::driver::{make_files, scratch, seed_caches};
use crate::report::{median, range};
use crate::scenario::Scenario;
use crate::server::{self, Kind, Server};
use crate::smaps;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const ALL: [&str; 5] = ["mu", "claude", "opencode", "omp", "pi"];
const TIMEOUT: Duration = Duration::from_secs(240);
const SAMPLE_EVERY: Duration = Duration::from_millis(10);
const KEY: &str = "sk-bench";

struct Launch {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

struct Ctx<'a> {
    mu: &'a Path,
    url: &'a str,
    prompt: &'a str,
    tmp: &'a Path,
}

/// Variables that would send a harness to a real provider or into the user's own setup.
const SCRUB: &[&str] = &[
    "OMP_PROFILE",
    "MU_PROFILE",
    "MU_DUMP_REQUESTS",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_CONTENT",
];

/// `name` on `PATH`, else in the tools directory (`$MU_BENCH_TOOLS`, default `~/.cache/mu-bench-tools`)
/// where harnesses without a system install live: `npm install --prefix <dir> <package>`.
fn which(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tools = std::env::var_os("MU_BENCH_TOOLS")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/mu-bench-tools")))
        .map(|d| d.join("node_modules/.bin"));
    std::env::split_paths(&path)
        .chain(tools)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .with_context(|| format!("`{name}` is not on PATH or in the tools directory"))
}

fn s(v: impl Into<String>) -> String {
    v.into()
}

pub fn omp_models_yml(url: &str) -> String {
    format!(
        "providers:\n  bench:\n    baseUrl: {url}\n    apiKey: BENCH_KEY\n    api: anthropic-messages\n    auth: apiKey\n    models:\n      - id: bench-1\n        name: Bench 1\n        contextWindow: 200000\n        maxTokens: 32000\n"
    )
}

pub fn pi_models_json(url: &str) -> String {
    serde_json::json!({"providers": {"bench": {
        "baseUrl": url,
        "api": "anthropic-messages",
        "apiKey": KEY,
        "models": [{"id": "bench-1", "contextWindow": 200000, "maxTokens": 32000}],
    }}})
    .to_string()
}

pub fn opencode_config(url: &str) -> String {
    serde_json::json!({
        "provider": {"anthropic": {"options": {"baseURL": format!("{url}/v1"), "apiKey": KEY}}},
        "permission": {"bash": "allow", "edit": "allow", "write": "allow", "webfetch": "allow"},
    })
    .to_string()
}

fn launch(name: &str, c: &Ctx) -> Result<Launch> {
    let home = c.tmp.join("home");
    let h = home.display().to_string();
    let base = vec![(s("HOME"), h.clone())];
    Ok(match name {
        "mu" => Launch {
            program: c.mu.to_path_buf(),
            args: vec![s("-p"), s(c.prompt), s("--model"), s(server::MODEL)],
            env: [
                base,
                vec![
                    (s("MU_HOME"), c.tmp.join("mu").display().to_string()),
                    (s("PI_CODING_AGENT_DIR"), home.join("agent").display().to_string()),
                    (s("MU_GATEWAY_URL"), s(c.url)),
                    (s("MU_GATEWAY_TOKEN"), s("bench")),
                ],
            ]
            .concat(),
        },
        "claude" => Launch {
            program: which("claude")?,
            args: vec![s("-p"), s(c.prompt), s("--dangerously-skip-permissions")],
            env: [
                base,
                vec![
                    (s("CLAUDE_CONFIG_DIR"), home.join("claude").display().to_string()),
                    (s("ANTHROPIC_BASE_URL"), s(c.url)),
                    (s("ANTHROPIC_API_KEY"), s(KEY)),
                    (s("DISABLE_AUTOUPDATER"), s("1")),
                    (s("DISABLE_TELEMETRY"), s("1")),
                ],
            ]
            .concat(),
        },
        "omp" => {
            std::fs::create_dir_all(home.join("agent"))?;
            std::fs::write(home.join("agent/models.yml"), omp_models_yml(c.url))?;
            Launch {
                program: which("omp")?,
                args: vec![s("-p"), s(c.prompt), s("--model"), s("bench/bench-1")],
                env: [
                    base,
                    vec![
                        (s("PI_CODING_AGENT_DIR"), home.join("agent").display().to_string()),
                        (s("BENCH_KEY"), s(KEY)),
                    ],
                ]
                .concat(),
            }
        }
        "pi" => {
            std::fs::create_dir_all(home.join("agent"))?;
            std::fs::write(home.join("agent/models.json"), pi_models_json(c.url))?;
            Launch {
                program: which("pi")?,
                args: vec![s("-p"), s(c.prompt), s("--model"), s("bench/bench-1")],
                env: [base, vec![(s("PI_CODING_AGENT_DIR"), home.join("agent").display().to_string())]].concat(),
            }
        }
        "opencode" => {
            let x = |d: &str| (format!("XDG_{d}_HOME"), c.tmp.join(d.to_lowercase()).display().to_string());
            Launch {
                program: which("opencode")?,
                args: vec![s("run"), s("-m"), s("anthropic/claude-sonnet-4-5"), s(c.prompt)],
                env: [
                    base,
                    vec![
                        x("CONFIG"),
                        x("DATA"),
                        x("CACHE"),
                        x("STATE"),
                        (s("OPENCODE_CONFIG_CONTENT"), opencode_config(c.url)),
                    ],
                ]
                .concat(),
            }
        }
        other => bail!("unknown harness `{other}` (known: {})", ALL.join(", ")),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct RunOut {
    pub harness: String,
    pub wall_ms: f64,
    pub exit_ok: bool,
    /// The harness got through every scripted step.
    pub completed: bool,
    pub peak_pss_kb: u64,
    pub peak_private_kb: u64,
    pub peak_procs: usize,
    pub model_requests: usize,
    pub tool_errors: usize,
    pub overruns: usize,
    pub log_tail: String,
}

pub async fn run_once(name: &str, sc: &Arc<Scenario>, mu: &Path) -> Result<RunOut> {
    let tmp = scratch()?;
    let r = run_in(name, sc, mu, &tmp).await;
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

async fn run_in(name: &str, sc: &Arc<Scenario>, mu: &Path, tmp: &Path) -> Result<RunOut> {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(tmp.join("home"))?;
    make_files(sc, &work)?;
    seed_caches(&tmp.join("mu"))?;
    let srv = Server::start_in(sc.clone(), Some(work.clone())).await?;
    let url = srv.url();
    let l = launch(name, &Ctx { mu, url: &url, prompt: &sc.turns[0].prompt, tmp })?;
    let log_path = tmp.join("out.log");
    let log = std::fs::File::create(&log_path)?;
    let mut cmd = Command::new(&l.program);
    cmd.args(&l.args).current_dir(&work).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).process_group(0);
    for k in SCRUB {
        cmd.env_remove(k);
    }
    cmd.envs(l.env.iter().map(|(k, v)| (k, v)));
    let start = Instant::now();
    let mut child = cmd.spawn().with_context(|| format!("starting {}", l.program.display()))?;
    let pid = child.id() as i32;
    let (mut peak_pss, mut peak_priv, mut peak_procs) = (0, 0, 0);
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break Some(st);
        }
        let procs = smaps::sample_tree(pid);
        let t = smaps::total(&procs);
        peak_pss = peak_pss.max(t.pss_kb);
        peak_priv = peak_priv.max(t.pss_anon_kb);
        peak_procs = peak_procs.max(procs.len());
        if start.elapsed() > TIMEOUT {
            break None;
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    };
    let wall = start.elapsed();
    // SAFETY: plain kill(2) on the process group this function created; stragglers must not outlive the run.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let _ = child.wait();
    let (completed, model_requests, tool_errors, overruns) = {
        let log = srv.log.lock().unwrap();
        (
            log.turns_done >= 1,
            log.requests.iter().filter(|r| r.kind == Kind::Main).count(),
            log.requests.iter().map(|r| r.tool_errors).sum(),
            log.overruns,
        )
    };
    let exit_ok = status.is_some_and(|s| s.success());
    let tail = if completed && exit_ok {
        String::new()
    } else {
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        let from = text.char_indices().rev().nth(400).map_or(0, |(i, _)| i);
        text[from..].to_string()
    };
    Ok(RunOut {
        harness: name.into(),
        wall_ms: wall.as_secs_f64() * 1000.0,
        exit_ok,
        completed,
        peak_pss_kb: peak_pss,
        peak_private_kb: peak_priv,
        peak_procs,
        model_requests,
        tool_errors,
        overruns,
        log_tail: tail,
    })
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub harness: String,
    pub runs: usize,
    pub completed: usize,
    pub wall_ms: f64,
    pub wall_range_ms: (f64, f64),
    pub peak_pss_mb: f64,
    pub peak_private_mb: f64,
    pub peak_procs: usize,
    pub model_requests: f64,
    pub tool_errors: f64,
}

pub fn summarize(runs: &[RunOut]) -> Vec<Row> {
    let mut names: Vec<&str> = Vec::new();
    for r in runs {
        if !names.contains(&r.harness.as_str()) {
            names.push(&r.harness);
        }
    }
    names
        .into_iter()
        .map(|n| {
            let rs: Vec<&RunOut> = runs.iter().filter(|r| r.harness == n).collect();
            let col = |f: &dyn Fn(&RunOut) -> f64| rs.iter().map(|r| f(r)).collect::<Vec<_>>();
            let wall = col(&|r| r.wall_ms);
            Row {
                harness: n.into(),
                runs: rs.len(),
                completed: rs.iter().filter(|r| r.completed && r.exit_ok).count(),
                wall_ms: median(&wall),
                wall_range_ms: range(&wall).unwrap_or((f64::NAN, f64::NAN)),
                peak_pss_mb: median(&col(&|r| r.peak_pss_kb as f64)) / 1024.0,
                peak_private_mb: median(&col(&|r| r.peak_private_kb as f64)) / 1024.0,
                peak_procs: rs.iter().map(|r| r.peak_procs).max().unwrap_or(0),
                model_requests: median(&col(&|r| r.model_requests as f64)),
                tool_errors: median(&col(&|r| r.tool_errors as f64)),
            }
        })
        .collect()
}

pub fn print(rows: &[Row]) {
    println!(
        "\n{:<10} {:>9} {:>22} {:>14} {:>16} {:>6} {:>9} {:>11}",
        "harness",
        "completed",
        "wall s, median (range)",
        "peak total MB",
        "peak private MB",
        "procs",
        "requests",
        "tool errors"
    );
    for r in rows {
        println!(
            "{:<10} {:>9} {:>22} {:>14.0} {:>16.0} {:>6} {:>9.0} {:>11.0}",
            r.harness,
            format!("{}/{}", r.completed, r.runs),
            format!("{:.2} ({:.2}-{:.2})", r.wall_ms / 1000.0, r.wall_range_ms.0 / 1000.0, r.wall_range_ms.1 / 1000.0),
            r.peak_pss_mb,
            r.peak_private_mb,
            r.peak_procs,
            r.model_requests,
            r.tool_errors
        );
    }
}

pub struct Args {
    pub mu: PathBuf,
    pub runs: usize,
    pub harnesses: Vec<String>,
    pub out: Option<PathBuf>,
    pub scenario: PathBuf,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut a = Args {
        mu: "target/release/mu".into(),
        runs: 5,
        harnesses: ALL.iter().map(|h| (*h).to_string()).collect(),
        out: None,
        scenario: PathBuf::new(),
    };
    let mut it = args.into_iter();
    let mut scenarios = Vec::new();
    while let Some(arg) = it.next() {
        let mut value = |n: &str| it.next().with_context(|| format!("{n} needs a value"));
        match arg.as_str() {
            "--mu" => a.mu = value("--mu")?.into(),
            "--runs" => a.runs = value("--runs")?.parse()?,
            "--harness" => a.harnesses = value("--harness")?.split(',').map(str::to_string).collect(),
            "--out" => a.out = Some(value("--out")?.into()),
            x if x.starts_with("--") => bail!("unknown option {x}"),
            _ => scenarios.push(arg),
        }
    }
    if scenarios.len() != 1 {
        bail!("compare takes exactly one scenario file");
    }
    a.scenario = scenarios.remove(0).into();
    if a.runs == 0 {
        bail!("--runs must be at least 1");
    }
    if let Some(h) = a.harnesses.iter().find(|h| !ALL.contains(&h.as_str())) {
        bail!("unknown harness `{h}` (known: {})", ALL.join(", "));
    }
    Ok(a)
}

pub async fn main(args: impl IntoIterator<Item = String>) -> Result<()> {
    let a = parse_args(args)?;
    let sc = Arc::new(Scenario::load(&a.scenario)?);
    if sc.turns.len() != 1 {
        bail!("compare runs a single prompt headless; `{}` has {} turns", sc.name, sc.turns.len());
    }
    let mu = std::fs::canonicalize(&a.mu).with_context(|| format!("{}", a.mu.display()))?;
    let mut runs = Vec::new();
    // Interleaved, so drift in the machine's state hits every harness alike.
    for i in 0..a.runs {
        for h in &a.harnesses {
            eprintln!("run {}/{}: {h}", i + 1, a.runs);
            let r = run_once(h, &sc, &mu).await?;
            if !(r.completed && r.exit_ok) {
                eprintln!(
                    "  {h} did not finish the scenario (exit ok: {}, steps served: {}); output tail: {:?}",
                    r.exit_ok, r.model_requests, r.log_tail
                );
            }
            runs.push(r);
        }
    }
    println!("scenario `{}`: one prompt, {} model steps, fake model, no network", sc.name, sc.turns[0].steps.len());
    let rows = summarize(&runs);
    print(&rows);
    if let Some(out) = &a.out {
        std::fs::write(
            out,
            serde_json::to_string_pretty(&serde_json::json!({"scenario": sc.name, "rows": rows, "runs": runs}))?,
        )?;
        eprintln!("wrote {}", out.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Args> {
        parse_args(a.iter().map(|x| x.to_string()))
    }

    #[test]
    fn arguments() {
        let a = p(&["--runs", "3", "--harness", "mu,omp", "s.toml"]).unwrap();
        assert_eq!((a.runs, a.harnesses.as_slice()), (3, &["mu".to_string(), "omp".to_string()][..]));
        assert_eq!(a.scenario, Path::new("s.toml"));
        assert_eq!(p(&["s.toml"]).unwrap().harnesses.len(), ALL.len());
        assert!(p(&[]).is_err());
        assert!(p(&["a.toml", "b.toml"]).is_err());
        assert!(p(&["--harness", "nope", "s.toml"]).is_err());
        assert!(p(&["--runs", "0", "s.toml"]).is_err());
    }

    #[test]
    fn configs_name_the_fake_server() {
        assert!(omp_models_yml("http://127.0.0.1:1").contains("baseUrl: http://127.0.0.1:1\n"));
        let oc: serde_json::Value = serde_json::from_str(&opencode_config("http://127.0.0.1:1")).unwrap();
        assert_eq!(oc["provider"]["anthropic"]["options"]["baseURL"], "http://127.0.0.1:1/v1");
        assert_eq!(oc["permission"]["bash"], "allow");
        let pi: serde_json::Value = serde_json::from_str(&pi_models_json("http://127.0.0.1:1")).unwrap();
        assert_eq!(pi["providers"]["bench"]["baseUrl"], "http://127.0.0.1:1");
        assert_eq!(pi["providers"]["bench"]["api"], "anthropic-messages");
    }

    #[test]
    fn mu_launch_is_headless_and_isolated() {
        let tmp = Path::new("/t");
        let l = launch("mu", &Ctx { mu: Path::new("/bin/mu"), url: "http://u", prompt: "do it", tmp }).unwrap();
        assert_eq!(l.args[..2], ["-p", "do it"]);
        let env = |k: &str| l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(env("MU_GATEWAY_URL"), Some("http://u"));
        assert_eq!(env("HOME"), Some("/t/home"));
        assert!(launch("nope", &Ctx { mu: Path::new("/bin/mu"), url: "", prompt: "", tmp }).is_err());
    }

    #[test]
    fn summary_takes_medians_per_harness() {
        let run = |h: &str, wall: f64, pss: u64, ok: bool| RunOut {
            harness: h.into(),
            wall_ms: wall,
            exit_ok: ok,
            completed: ok,
            peak_pss_kb: pss,
            peak_private_kb: pss / 2,
            peak_procs: 2,
            model_requests: 10,
            tool_errors: 0,
            overruns: 0,
            log_tail: String::new(),
        };
        let rows = summarize(&[
            run("a", 1000.0, 2048, true),
            run("b", 5000.0, 4096, true),
            run("a", 3000.0, 4096, true),
            run("a", 2000.0, 1024, false),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].harness.as_str(), rows[0].runs, rows[0].completed), ("a", 3, 2));
        assert_eq!(rows[0].wall_ms, 2000.0);
        assert_eq!(rows[0].wall_range_ms, (1000.0, 3000.0));
        assert_eq!(rows[0].peak_pss_mb, 2.0);
    }
}
