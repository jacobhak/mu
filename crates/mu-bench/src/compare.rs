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
    /// Fresh for every run: working directory, mu's own state.
    tmp: &'a Path,
    /// The harness's `HOME`, kept across runs so one-time first-run setup is not charged to each run.
    home: &'a Path,
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
        "providers:\n  bench:\n    baseUrl: {url}\n    apiKey: {KEY}\n    api: anthropic-messages\n    auth: apiKey\n    models:\n      - id: bench-1\n        name: Bench 1\n        contextWindow: 200000\n        maxTokens: 32000\n"
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
    let home = c.home.to_path_buf();
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
                    (s("PI_CODING_AGENT_DIR"), c.tmp.join("agent").display().to_string()),
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
            // Per session, not in the shared HOME: it holds the URL of this run's fake server.
            std::fs::create_dir_all(c.tmp.join("agent"))?;
            std::fs::write(c.tmp.join("agent/models.yml"), omp_models_yml(c.url))?;
            Launch {
                program: which("omp")?,
                args: vec![s("-p"), s(c.prompt), s("--model"), s("bench/bench-1")],
                env: [
                    base,
                    vec![
                        (s("PI_CODING_AGENT_DIR"), c.tmp.join("agent").display().to_string()),
                        (s("BENCH_KEY"), s(KEY)),
                    ],
                ]
                .concat(),
            }
        }
        "pi" => {
            std::fs::create_dir_all(c.tmp.join("agent"))?;
            std::fs::write(c.tmp.join("agent/models.json"), pi_models_json(c.url))?;
            Launch {
                program: which("pi")?,
                args: vec![s("-p"), s(c.prompt), s("--model"), s("bench/bench-1")],
                env: [base, vec![(s("PI_CODING_AGENT_DIR"), c.tmp.join("agent").display().to_string())]].concat(),
            }
        }
        "opencode" => {
            let x = |d: &str| (format!("XDG_{d}_HOME"), home.join("xdg").join(d.to_lowercase()).display().to_string());
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
    /// Sessions running at the same time as this one, itself included.
    pub sessions: usize,
    pub wall_ms: f64,
    /// User plus system CPU time of the process and the children it waited for.
    pub cpu_ms: f64,
    pub exit_ok: bool,
    /// The harness got through every scripted step.
    pub completed: bool,
    pub peak_pss_kb: u64,
    pub peak_private_kb: u64,
    /// The last sample before the process exited: what it still held at the end of its work.
    pub final_pss_kb: u64,
    pub final_private_kb: u64,
    pub peak_procs: usize,
    pub model_requests: usize,
    pub tool_errors: usize,
    pub overruns: usize,
    /// Peak resident set of the largest process, as the kernel reports it (shared pages count in full).
    pub max_rss_kb: u64,
    /// Bytes of tool output the harness sent back to the model, and files it wrote: the same work should
    /// give about the same numbers for every harness.
    pub result_kb: u64,
    pub files_written: usize,
    pub log_tail: String,
}

pub async fn run_once(name: &str, sc: &Arc<Scenario>, mu: &Path, home_base: &Path, sessions: usize) -> Result<RunOut> {
    let tmp = scratch()?;
    let home = home_base.join(name);
    let r = run_in(name, sc, mu, &tmp, &home, sessions).await;
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

/// `k` sessions of one harness started together and sharing its `HOME`, as several terminals of one user do.
pub async fn run_group(name: &str, k: usize, sc: &Arc<Scenario>, mu: &Path, home_base: &Path) -> Result<Vec<RunOut>> {
    let tasks: Vec<_> = (0..k)
        .map(|_| {
            let (n, sc, mu, hb) = (name.to_string(), sc.clone(), mu.to_path_buf(), home_base.to_path_buf());
            tokio::spawn(async move { run_once(&n, &sc, &mu, &hb, k).await })
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await??);
    }
    Ok(out)
}

fn cpu_ms(ru: &libc::rusage) -> f64 {
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

async fn run_in(name: &str, sc: &Arc<Scenario>, mu: &Path, tmp: &Path, home: &Path, sessions: usize) -> Result<RunOut> {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(home)?;
    make_files(sc, &work)?;
    seed_caches(&tmp.join("mu"))?;
    let srv = Server::start_in(sc.clone(), Some(work.clone())).await?;
    let url = srv.url();
    let l = launch(name, &Ctx { mu, url: &url, prompt: &sc.turns[0].prompt, tmp, home })?;
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
    let mut last = smaps::Mem::default();
    let (status, max_rss_kb, cpu) = loop {
        // SAFETY: wait4 on our own child with valid out-pointers; `rusage` is plain data.
        let (mut st, mut ru) = (0, unsafe { std::mem::zeroed::<libc::rusage>() });
        let r = unsafe { libc::wait4(pid, &mut st, libc::WNOHANG, &mut ru) };
        if r == pid {
            break (Some(st), ru.ru_maxrss as u64, cpu_ms(&ru));
        }
        if r < 0 {
            break (None, 0, 0.0);
        }
        let procs = smaps::sample_tree(pid);
        let t = smaps::total(&procs);
        peak_pss = peak_pss.max(t.pss_kb);
        peak_priv = peak_priv.max(t.pss_anon_kb);
        peak_procs = peak_procs.max(procs.len());
        if t.pss_kb > 0 {
            last = t;
        }
        if start.elapsed() > TIMEOUT {
            break (None, 0, 0.0);
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    };
    let wall = start.elapsed();
    // SAFETY: plain kill(2) on the process group this function created; stragglers must not outlive the run.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    if status.is_none() {
        let _ = child.wait();
    }
    let (completed, model_requests, tool_errors, overruns, result_bytes) = {
        let log = srv.log.lock().unwrap();
        (
            log.turns_done >= 1,
            log.requests.iter().filter(|r| r.kind == Kind::Main).count(),
            log.requests.iter().map(|r| r.tool_errors).sum(),
            log.overruns,
            log.requests.iter().map(|r| r.tool_result_bytes as u64).sum::<u64>(),
        )
    };
    let exit_ok = status.is_some_and(|s| libc::WIFEXITED(s) && libc::WEXITSTATUS(s) == 0);
    let files_written = std::fs::read_dir(work.join("out")).map_or(0, |d| d.count());
    let tail = if completed && exit_ok {
        String::new()
    } else {
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        let from = text.char_indices().rev().nth(400).map_or(0, |(i, _)| i);
        text[from..].to_string()
    };
    Ok(RunOut {
        harness: name.into(),
        sessions,
        wall_ms: wall.as_secs_f64() * 1000.0,
        cpu_ms: cpu,
        exit_ok,
        completed,
        peak_pss_kb: peak_pss,
        peak_private_kb: peak_priv,
        final_pss_kb: last.pss_kb,
        final_private_kb: last.pss_anon_kb,
        peak_procs,
        model_requests,
        tool_errors,
        overruns,
        max_rss_kb,
        result_kb: result_bytes / 1024,
        files_written,
        log_tail: tail,
    })
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub harness: String,
    pub sessions: usize,
    pub runs: usize,
    pub completed: usize,
    pub wall_ms: f64,
    pub wall_range_ms: (f64, f64),
    pub cpu_s: f64,
    pub final_private_mb: f64,
    pub final_pss_mb: f64,
    pub peak_pss_mb: f64,
    pub peak_private_mb: f64,
    pub max_rss_mb: f64,
    pub peak_procs: usize,
    pub model_requests: f64,
    pub tool_errors: f64,
    pub result_kb: f64,
    pub files_written: f64,
}

/// One row per harness and session count, in order of first appearance.
pub fn summarize(runs: &[RunOut]) -> Vec<Row> {
    let mut keys: Vec<(&str, usize)> = Vec::new();
    for r in runs {
        let k = (r.harness.as_str(), r.sessions);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys.into_iter()
        .map(|(n, sessions)| {
            let rs: Vec<&RunOut> = runs.iter().filter(|r| r.harness == n && r.sessions == sessions).collect();
            let col = |f: &dyn Fn(&RunOut) -> f64| rs.iter().map(|r| f(r)).collect::<Vec<_>>();
            let wall = col(&|r| r.wall_ms);
            Row {
                harness: n.into(),
                sessions,
                runs: rs.len(),
                completed: rs.iter().filter(|r| r.completed && r.exit_ok).count(),
                wall_ms: median(&wall),
                wall_range_ms: range(&wall).unwrap_or((f64::NAN, f64::NAN)),
                cpu_s: median(&col(&|r| r.cpu_ms)) / 1000.0,
                final_private_mb: median(&col(&|r| r.final_private_kb as f64)) / 1024.0,
                final_pss_mb: median(&col(&|r| r.final_pss_kb as f64)) / 1024.0,
                peak_pss_mb: median(&col(&|r| r.peak_pss_kb as f64)) / 1024.0,
                peak_private_mb: median(&col(&|r| r.peak_private_kb as f64)) / 1024.0,
                max_rss_mb: median(&col(&|r| r.max_rss_kb as f64)) / 1024.0,
                peak_procs: rs.iter().map(|r| r.peak_procs).max().unwrap_or(0),
                model_requests: median(&col(&|r| r.model_requests as f64)),
                tool_errors: median(&col(&|r| r.tool_errors as f64)),
                result_kb: median(&col(&|r| r.result_kb as f64)),
                files_written: median(&col(&|r| r.files_written as f64)),
            }
        })
        .collect()
}

pub fn print(rows: &[Row]) {
    let solo: Vec<&Row> = rows.iter().filter(|r| r.sessions == 1).collect();
    let multi: Vec<&Row> = rows.iter().filter(|r| r.sessions > 1).collect();
    if !solo.is_empty() {
        println!("\none session at a time");
        println!(
            "{:<9} {:>8} {:>21} {:>6} {:>12} {:>12} {:>9} {:>6} {:>9} {:>6}",
            "harness",
            "finished",
            "wall s, median (range)",
            "cpu s",
            "private MB",
            "total MB",
            "peak MB",
            "steps",
            "result kB",
            "files"
        );
        for r in solo {
            println!(
                "{:<9} {:>8} {:>21} {:>6.2} {:>12.0} {:>12.0} {:>9.0} {:>6.0} {:>9.0} {:>6.0}",
                r.harness,
                format!("{}/{}", r.completed, r.runs),
                format!(
                    "{:.2} ({:.2}-{:.2})",
                    r.wall_ms / 1000.0,
                    r.wall_range_ms.0 / 1000.0,
                    r.wall_range_ms.1 / 1000.0
                ),
                r.cpu_s,
                r.final_private_mb,
                r.final_pss_mb,
                r.peak_pss_mb,
                r.model_requests,
                r.result_kb,
                r.files_written
            );
        }
    }
    if let Some(k) = multi.first().map(|r| r.sessions) {
        println!("\n{k} sessions at once, per session");
        println!(
            "{:<9} {:>8} {:>21} {:>12} {:>12} {:>9}",
            "harness", "finished", "wall s, median", "private MB", "total MB", "peak MB"
        );
        for r in multi {
            println!(
                "{:<9} {:>8} {:>21.2} {:>12.0} {:>12.0} {:>9.0}",
                r.harness,
                format!("{}/{}", r.completed, r.runs),
                r.wall_ms / 1000.0,
                r.final_private_mb,
                r.final_pss_mb,
                r.peak_pss_mb
            );
        }
    }
    println!(
        "\nprivate and total are what the process tree still held in its last sample before exit; total counts shared"
    );
    println!("pages divided among the processes sharing them, so with several sessions it is each one's real share.");
    println!("peak is the same total at its highest. Models, gateways and other shared daemons are not included.");
}

pub struct Args {
    pub mu: PathBuf,
    pub runs: usize,
    pub sessions: usize,
    pub harnesses: Vec<String>,
    pub out: Option<PathBuf>,
    pub scenario: PathBuf,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut a = Args {
        mu: "target/release/mu".into(),
        runs: 5,
        sessions: 3,
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
            "--sessions" => a.sessions = value("--sessions")?.parse()?,
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
    if a.runs == 0 || a.sessions == 0 {
        bail!("--runs and --sessions must be at least 1");
    }
    if let Some(h) = a.harnesses.iter().find(|h| !ALL.contains(&h.as_str())) {
        bail!("unknown harness `{h}` (known: {})", ALL.join(", "));
    }
    Ok(a)
}

fn note_failure(r: &RunOut) {
    if !(r.completed && r.exit_ok) {
        eprintln!(
            "  {} ({} at once) did not finish the scenario (exit ok: {}, steps served: {}); output tail: {:?}",
            r.harness, r.sessions, r.exit_ok, r.model_requests, r.log_tail
        );
    }
}

pub async fn main(args: impl IntoIterator<Item = String>) -> Result<()> {
    let a = parse_args(args)?;
    let sc = Arc::new(Scenario::load(&a.scenario)?);
    if sc.turns.len() != 1 {
        bail!("compare runs a single prompt headless; `{}` has {} turns", sc.name, sc.turns.len());
    }
    let mu = std::fs::canonicalize(&a.mu).with_context(|| format!("{}", a.mu.display()))?;
    let mut runs = Vec::new();
    let home_base = scratch()?;
    // One untimed run each, so first-run setup (databases, native libraries) is not charged to the measured runs.
    for h in &a.harnesses {
        eprintln!("warm-up: {h}");
        note_failure(&run_once(h, &sc, &mu, &home_base, 1).await?);
    }
    // Interleaved, so drift in the machine's state hits every harness alike.
    for i in 0..a.runs {
        for h in &a.harnesses {
            eprintln!("run {}/{}: {h}", i + 1, a.runs);
            let r = run_once(h, &sc, &mu, &home_base, 1).await?;
            note_failure(&r);
            runs.push(r);
            if a.sessions > 1 {
                for r in run_group(h, a.sessions, &sc, &mu, &home_base).await? {
                    note_failure(&r);
                    runs.push(r);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&home_base);
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
        let l = launch(
            "mu",
            &Ctx { mu: Path::new("/bin/mu"), url: "http://u", prompt: "do it", tmp, home: Path::new("/t/home") },
        )
        .unwrap();
        assert_eq!(l.args[..2], ["-p", "do it"]);
        let env = |k: &str| l.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(env("MU_GATEWAY_URL"), Some("http://u"));
        assert_eq!(env("HOME"), Some("/t/home"));
        let nope = Ctx { mu: Path::new("/bin/mu"), url: "", prompt: "", tmp, home: tmp };
        assert!(launch("nope", &nope).is_err());
    }

    #[test]
    fn summary_groups_by_harness_and_session_count() {
        let run = |h: &str, sessions: usize, wall: f64, pss: u64, ok: bool| RunOut {
            harness: h.into(),
            sessions,
            wall_ms: wall,
            cpu_ms: wall / 2.0,
            exit_ok: ok,
            completed: ok,
            peak_pss_kb: pss,
            peak_private_kb: pss / 2,
            final_pss_kb: pss / 2,
            final_private_kb: pss / 4,
            peak_procs: 2,
            model_requests: 10,
            tool_errors: 0,
            overruns: 0,
            max_rss_kb: pss,
            result_kb: 100,
            files_written: 4,
            log_tail: String::new(),
        };
        let rows = summarize(&[
            run("a", 1, 1000.0, 2048, true),
            run("b", 1, 5000.0, 4096, true),
            run("a", 1, 3000.0, 4096, true),
            run("a", 1, 2000.0, 1024, false),
            run("a", 3, 9000.0, 8192, true),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].harness.as_str(), rows[0].sessions, rows[0].runs, rows[0].completed), ("a", 1, 3, 2));
        assert_eq!(rows[0].wall_ms, 2000.0);
        assert_eq!(rows[0].wall_range_ms, (1000.0, 3000.0));
        assert_eq!(rows[0].peak_pss_mb, 2.0);
        assert_eq!(rows[0].cpu_s, 1.0);
        assert_eq!(rows[0].final_pss_mb, 1.0);
        assert_eq!((rows[2].harness.as_str(), rows[2].sessions), ("a", 3));
        assert_eq!(rows[2].final_private_mb, 2.0);
    }

    #[test]
    fn session_count_argument() {
        assert_eq!(p(&["s.toml"]).unwrap().sessions, 3);
        assert_eq!(p(&["--sessions", "1", "s.toml"]).unwrap().sessions, 1);
        assert!(p(&["--sessions", "0", "s.toml"]).is_err());
    }
}
