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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

fn is_node_script(head: &str) -> bool {
    head.lines().next().is_some_and(|l| l.starts_with("#!") && l.contains("node"))
}

/// `src` copied to `dir/name`, unless a copy of the same size is already there.
fn copy_into(src: &Path, dir: &Path, name: &std::ffi::OsStr) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let dst = dir.join(name);
    let len = std::fs::metadata(src)?.len();
    if std::fs::metadata(&dst).is_ok_and(|m| m.len() == len) {
        return Ok(dst);
    }
    let part = dir.join(format!(".{}.{}", name.to_string_lossy(), std::process::id()));
    std::fs::copy(src, &part).with_context(|| format!("copying {}", src.display()))?;
    std::fs::rename(&part, &dst)?;
    Ok(dst)
}

/// Runs the launch from private copies of its executable files (for a node script, of `node`). Every process that
/// maps a file shares its pages and `Pss` divides them among those processes, so another instance of the same
/// harness running on the machine (a real session, a daemon, the interpreter of some other tool) would lower the
/// numbers. A copy is a different file, so only the benchmark's own sessions share it.
fn privatize(l: &mut Launch, bins: &Path) -> Result<()> {
    use std::io::Read;
    let name = l.program.file_name().context("program has no file name")?.to_owned();
    let src = std::fs::canonicalize(&l.program)?;
    let mut head = Vec::new();
    std::fs::File::open(&src)?.take(256).read_to_end(&mut head)?;
    if is_node_script(&String::from_utf8_lossy(&head)) {
        let node = std::fs::canonicalize(which("node")?)?;
        l.args.insert(0, src.display().to_string());
        l.program = copy_into(&node, bins, "node".as_ref())?;
    } else {
        l.program = copy_into(&src, bins, &name)?;
    }
    Ok(())
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
    /// The most that all the sessions started together held at one instant, summed over every process of every
    /// session. For a single session it is that session's own peak.
    pub group_peak_pss_kb: u64,
    pub group_peak_private_kb: u64,
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

/// Root pids of the sessions of one group that are running now.
type Live = Arc<Mutex<Vec<i32>>>;

pub async fn run_once(name: &str, sc: &Arc<Scenario>, mu: &Path, home_base: &Path, sessions: usize) -> Result<RunOut> {
    once(name, sc, mu, home_base, sessions, &Live::default()).await
}

async fn once(
    name: &str,
    sc: &Arc<Scenario>,
    mu: &Path,
    home_base: &Path,
    sessions: usize,
    live: &Live,
) -> Result<RunOut> {
    let tmp = scratch()?;
    let home = home_base.join(name);
    let r = run_in(name, sc, mu, &tmp, &home, sessions, live).await;
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

/// Peak of the memory of all live sessions together, sampled until `stop`.
async fn group_sampler(live: Live, stop: Arc<AtomicBool>) -> smaps::Mem {
    let mut peak = smaps::Mem::default();
    while !stop.load(Ordering::Relaxed) {
        let roots = live.lock().unwrap().clone();
        if !roots.is_empty() {
            let m = smaps::sample_roots(&roots);
            peak.pss_kb = peak.pss_kb.max(m.pss_kb);
            peak.pss_anon_kb = peak.pss_anon_kb.max(m.pss_anon_kb);
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
    peak
}

/// `k` sessions of one harness started together and sharing its `HOME`, as several terminals of one user do.
pub async fn run_group(name: &str, k: usize, sc: &Arc<Scenario>, mu: &Path, home_base: &Path) -> Result<Vec<RunOut>> {
    let live = Live::default();
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = tokio::spawn(group_sampler(live.clone(), stop.clone()));
    let tasks: Vec<_> = (0..k)
        .map(|_| {
            let (n, sc, mu, hb, live) =
                (name.to_string(), sc.clone(), mu.to_path_buf(), home_base.to_path_buf(), live.clone());
            tokio::spawn(async move { once(&n, &sc, &mu, &hb, k, &live).await })
        })
        .collect();
    let (mut out, mut failure) = (Vec::new(), None);
    for t in tasks {
        match t.await {
            Ok(Ok(r)) => out.push(r),
            Ok(Err(e)) => failure = Some(e),
            Err(e) => failure = Some(e.into()),
        }
    }
    stop.store(true, Ordering::Relaxed);
    let peak = sampler.await?;
    if let Some(e) = failure {
        return Err(e);
    }
    for r in &mut out {
        r.group_peak_pss_kb = peak.pss_kb;
        r.group_peak_private_kb = peak.pss_anon_kb;
    }
    Ok(out)
}

fn cpu_ms(ru: &libc::rusage) -> f64 {
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

async fn run_in(
    name: &str,
    sc: &Arc<Scenario>,
    mu: &Path,
    tmp: &Path,
    home: &Path,
    sessions: usize,
    live: &Live,
) -> Result<RunOut> {
    let work = tmp.join("work");
    std::fs::create_dir_all(&work)?;
    std::fs::create_dir_all(home)?;
    make_files(sc, &work)?;
    seed_caches(&tmp.join("mu"))?;
    let srv = Server::start_in(sc.clone(), Some(work.clone())).await?;
    let url = srv.url();
    let mut l = launch(name, &Ctx { mu, url: &url, prompt: &sc.turns[0].prompt, tmp, home })?;
    privatize(&mut l, &home.parent().unwrap_or(home).join("bin"))?;
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
    live.lock().unwrap().push(pid);
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
    live.lock().unwrap().retain(|p| *p != pid);
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
        group_peak_pss_kb: peak_pss,
        group_peak_private_kb: peak_priv,
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
    /// Peak of all the sessions together (see `RunOut::group_peak_pss_kb`).
    pub group_peak_pss_mb: f64,
    pub group_peak_private_mb: f64,
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
                group_peak_pss_mb: median(&col(&|r| r.group_peak_pss_kb as f64)) / 1024.0,
                group_peak_private_mb: median(&col(&|r| r.group_peak_private_kb as f64)) / 1024.0,
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
    let mut counts: Vec<usize> = multi.iter().map(|r| r.sessions).collect();
    counts.sort_unstable();
    counts.dedup();
    for k in &counts {
        println!("\n{k} sessions at once, per session");
        println!(
            "{:<9} {:>8} {:>21} {:>12} {:>12} {:>9}",
            "harness", "finished", "wall s, median", "private MB", "total MB", "peak MB"
        );
        for r in multi.iter().filter(|r| r.sessions == *k) {
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
    if !counts.is_empty() {
        print_scaling(rows, &counts, "total", |r| r.group_peak_pss_mb);
        print_scaling(rows, &counts, "private", |r| r.group_peak_private_mb);
    }
    println!(
        "\nprivate and total are what the process tree still held in its last sample before exit; total counts shared"
    );
    println!("pages divided among the processes sharing them, so with several sessions it is each one's real share.");
    println!("peak is the same total at its highest.");
    println!(
        "all sessions together is the peak of the memory summed over every process of every live session at one instant."
    );
    println!(
        "\nCAVEAT: mu is measured WITHOUT its gateway. mu normally reaches models through a shared `omp auth-broker` +"
    );
    println!("`omp auth-gateway` pair: about 306 MB total and 216 MB private idle,");
    println!("shared by every mu session, measured separately");
    println!(
        "and NOT in the mu rows above. With it, a single mu session costs a little more than claude and less than omp; the saving"
    );
    println!(
        "appears only with several sessions. The other harnesses call their providers directly. No model, and no other"
    );
    println!("shared daemon, is included for any harness.");
}

/// All sessions together: the peak of the memory summed over every process of every session at one instant, for each
/// session count, and what each further session costs, `(sum at the most sessions - sum of one) / (sessions - 1)`.
fn print_scaling(rows: &[Row], counts: &[usize], what: &str, value: impl Fn(&Row) -> f64) {
    let mut harnesses: Vec<&str> = Vec::new();
    for r in rows {
        if !harnesses.contains(&r.harness.as_str()) {
            harnesses.push(&r.harness);
        }
    }
    let last = *counts.last().unwrap();
    println!("\nall sessions together, peak {what} MB over every process of every session");
    print!("{:<9} {:>9}", "harness", "1 session");
    for k in counts {
        print!(" {:>10}", format!("{k} at once"));
    }
    println!(" {:>13} {:>10}", "per further", "share of 1");
    for h in harnesses {
        let at = |k: usize| rows.iter().find(|r| r.harness == h && r.sessions == k).map(&value);
        let Some(one) = at(1) else { continue };
        print!("{h:<9} {one:>9.0}");
        for &k in counts {
            match at(k) {
                Some(v) => print!(" {v:>10.0}"),
                None => print!(" {:>10}", "-"),
            }
        }
        match at(last) {
            Some(v) if one > 0.0 => {
                let each = (v - one) / (last - 1) as f64;
                println!(" {each:>13.1} {:>9.0}%", 100.0 * each / one);
            }
            _ => println!(),
        }
    }
}

pub struct Args {
    pub mu: PathBuf,
    pub runs: usize,
    /// Session counts above 1 to run at once, besides the single session every round starts with.
    pub sessions: Vec<usize>,
    pub harnesses: Vec<String>,
    pub out: Option<PathBuf>,
    pub scenario: PathBuf,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut a = Args {
        mu: "target/release/mu".into(),
        runs: 5,
        sessions: vec![3],
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
            "--sessions" => {
                a.sessions = value("--sessions")?.split(',').map(str::parse).collect::<Result<_, _>>()?;
                a.sessions.sort_unstable();
                a.sessions.dedup();
            }
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
    if a.runs == 0 || a.sessions.contains(&0) {
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
            for &k in a.sessions.iter().filter(|&&k| k > 1) {
                for r in run_group(h, k, &sc, &mu, &home_base).await? {
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
    fn executables_are_copied_so_their_pages_are_not_shared() {
        use std::os::unix::fs::MetadataExt;
        assert!(is_node_script("#!/usr/bin/env node\nconsole.log(1)"));
        assert!(!is_node_script("#!/bin/sh\necho node"));
        assert!(!is_node_script("\x7fELF"));
        let dir = scratch().unwrap();
        let src = dir.join("tool");
        std::fs::write(&src, b"\x7fELF binary").unwrap();
        let mut l = Launch { program: src.clone(), args: vec![s("-p")], env: vec![] };
        privatize(&mut l, &dir.join("bin")).unwrap();
        assert_ne!(l.program, src);
        assert_eq!(std::fs::read(&l.program).unwrap(), b"\x7fELF binary");
        assert_ne!(std::fs::metadata(&l.program).unwrap().ino(), std::fs::metadata(&src).unwrap().ino());
        assert_eq!(l.args, ["-p"]);
        let first = l.program.clone();
        let mut again = Launch { program: src, args: vec![], env: vec![] };
        privatize(&mut again, &dir.join("bin")).unwrap();
        assert_eq!(again.program, first, "an existing copy is reused");
        let _ = std::fs::remove_dir_all(&dir);
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
            group_peak_pss_kb: pss * u64::try_from(sessions).unwrap(),
            group_peak_private_kb: pss / 2,
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
        assert_eq!(rows[2].group_peak_pss_mb, 24.0);
    }

    #[test]
    fn session_count_argument() {
        assert_eq!(p(&["s.toml"]).unwrap().sessions, vec![3]);
        assert_eq!(p(&["--sessions", "1", "s.toml"]).unwrap().sessions, vec![1]);
        assert_eq!(p(&["--sessions", "8,2,4,2", "s.toml"]).unwrap().sessions, vec![2, 4, 8]);
        assert!(p(&["--sessions", "0", "s.toml"]).is_err());
        assert!(p(&["--sessions", "2,0", "s.toml"]).is_err());
        assert!(p(&["--sessions", "x", "s.toml"]).is_err());
    }
}
