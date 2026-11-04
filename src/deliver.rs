//! `mu deliver`: approve and implement a parked plan, review it against the plan, send blocking findings back to the implementer session, then have it commit.

use crate::agent::DeliverFn;
use crate::{notify, plans, review, session};
use mu_common::paths::{encode_cwd, mu_home, self_exe};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;

#[derive(clap::Args)]
pub struct DeliverArgs {
    /// Plan id; with --loop, the id of a delivery to restart.
    #[arg(required_unless_present = "loop_only")]
    pub id: Option<String>,
    /// Create a worktree next to the checkout, on a new branch plan/<id> from upstream, and deliver there.
    #[arg(long, conflicts_with = "loop_only")]
    pub worktree: bool,
    /// Review and fix only, for work that already has an implementer session.
    #[arg(long = "loop")]
    pub loop_only: bool,
    /// With --loop and no id: the implementer session (file or id).
    #[arg(long, requires = "loop_only", conflicts_with = "id")]
    pub session: Option<PathBuf>,
    /// With --loop and no id: the commit the work started from.
    #[arg(long, requires = "loop_only", conflicts_with = "id")]
    pub base: Option<String>,
    /// With --loop and no id: the plan to review against (none: correctness and quality only).
    #[arg(long, requires = "loop_only", conflicts_with = "id")]
    pub plan: Option<PathBuf>,
    /// Most fix rounds in this run.
    #[arg(long, default_value_t = 2)]
    pub max_fix: u32,
    /// Session to notify when done; default $PI_SESSION_FILE when set.
    #[arg(long)]
    pub notify: Option<PathBuf>,
    /// Run in the background, logging to <record dir>/log.
    #[arg(long)]
    pub detach: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    #[default]
    Running,
    Pass,
    Fail,
    Error,
    Failed,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Running => "running",
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Error => "error",
            Outcome::Failed => "failed",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Record {
    pub id: String,
    pub plan: Option<String>,
    pub plan_path: Option<PathBuf>,
    pub title: String,
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub created_worktree: bool,
    pub branch: String,
    pub base: String,
    pub head: Option<String>,
    pub session: Option<PathBuf>,
    pub result: Outcome,
    pub verdict: Option<String>,
    pub reviews: u32,
    pub fixes: u32,
    pub max_fix: u32,
    pub last_review: Option<PathBuf>,
    pub last_report: Option<PathBuf>,
    pub leftovers: Vec<String>,
    pub error: Option<String>,
    pub notified: Option<String>,
    pub pid: u32,
    pub started: String,
    pub finished: Option<String>,
}

async fn deliveries_dir(cwd: &Path) -> PathBuf {
    mu_home().join("deliveries").join(encode_cwd(&plans::main_checkout(cwd).await))
}

fn save(dir: &Path, r: &Record) {
    let path = dir.join("delivery.json");
    let res = std::fs::create_dir_all(dir)
        .map_err(|e| e.to_string())
        .and_then(|()| serde_json::to_vec_pretty(r).map_err(|e| e.to_string()))
        .and_then(|b| mu_common::fs::write_atomic(&path, &b).map_err(|e| e.to_string()));
    if let Err(e) = res {
        eprintln!("mu deliver: cannot write {}: {e}", path.display());
    }
}

fn load(dir: &Path) -> Result<Record, String> {
    let p = dir.join("delivery.json");
    let raw = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", p.display()))
}

#[allow(async_fn_in_trait)]
pub(crate) trait Steps {
    /// `mu plan approve <id>` in `dir`, stdout to `out`. Returns the implementer session whenever the
    /// approval recorded one, even if the run then failed or was stopped.
    async fn implement(&mut self, dir: &Path, id: &str, out: &Path) -> (Option<PathBuf>, Result<(), String>);
    /// `mu review --base <base> [--plan <plan>] --out <out>` in `dir`; the loop reads `out`.
    async fn review(&mut self, dir: &Path, base: &str, plan: Option<&Path>, out: &Path);
    /// `mu --resume <session> -p <prompt>` in `dir`, stdout to `out`.
    async fn resume(&mut self, dir: &Path, session: &Path, prompt: &str, out: &Path) -> Result<(), String>;
    /// `mu notify`; the outcome line.
    async fn notify(&mut self, dir: &Path, session: &Path, text: &str) -> String;
}

pub(crate) struct MuSteps {
    pub cmd: Vec<OsString>,
}

impl MuSteps {
    async fn mu(&self, dir: &Path, args: &[&OsStr], stdout: Option<&Path>) -> Result<(), String> {
        let out = match stdout {
            Some(p) => Stdio::from(File::create(p).map_err(|e| format!("cannot write {}: {e}", p.display()))?),
            None => Stdio::null(),
        };
        let what = format!("mu {}", args.first().map(|a| a.to_string_lossy()).unwrap_or_default());
        let status = tokio::process::Command::new(&self.cmd[0])
            .args(&self.cmd[1..])
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(Stdio::inherit())
            .status()
            .await
            .map_err(|e| format!("cannot run {what}: {e}"))?;
        match status.code() {
            Some(0) => Ok(()),
            Some(c) => Err(format!("{what} exited {c}")),
            None => Err(format!("{what} killed by a signal")),
        }
    }
}

impl Steps for MuSteps {
    async fn implement(&mut self, dir: &Path, id: &str, out: &Path) -> (Option<PathBuf>, Result<(), String>) {
        let run = self.mu(dir, &["plan".as_ref(), "approve".as_ref(), id.as_ref()], Some(out)).await;
        let session = plans::store_for(dir).await.load(id).ok().and_then(|(m, _)| m.approved.and_then(|a| a.session));
        let run = match (run, &session) {
            (Ok(()), None) => Err("plan approved but no implementer session was recorded".to_string()),
            (r, _) => r,
        };
        (session, run)
    }

    async fn review(&mut self, dir: &Path, base: &str, plan: Option<&Path>, out: &Path) {
        let mut args: Vec<&OsStr> = vec!["review".as_ref(), "--base".as_ref(), base.as_ref()];
        if let Some(p) = plan {
            args.push("--plan".as_ref());
            args.push(p.as_os_str());
        }
        args.push("--out".as_ref());
        args.push(out.as_os_str());
        // The loop decides from the report, not the exit status.
        let _ = self.mu(dir, &args, None).await;
    }

    async fn resume(&mut self, dir: &Path, session: &Path, prompt: &str, out: &Path) -> Result<(), String> {
        let args: [&OsStr; 4] = ["--resume".as_ref(), session.as_os_str(), "-p".as_ref(), prompt.as_ref()];
        self.mu(dir, &args, Some(out)).await
    }

    async fn notify(&mut self, dir: &Path, session: &Path, text: &str) -> String {
        match notify::send(session, Some("mu deliver"), text, dir) {
            Ok(l) => l,
            Err(e) => format!("not sent: {e}"),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ReviewOut {
    verdict: String,
    error: Option<String>,
    summary: String,
    findings: Vec<ReviewFinding>,
}

#[derive(Deserialize)]
struct ReviewFinding {
    #[serde(flatten)]
    finding: review::Finding,
    #[serde(default)]
    blocking: bool,
}

fn read_review(path: &Path) -> ReviewOut {
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str::<ReviewOut>(&s).map_err(|e| e.to_string()));
    match parsed {
        Ok(mut r) => {
            if r.verdict.is_empty() {
                r.verdict = "error".into();
            }
            r
        }
        Err(e) => ReviewOut {
            verdict: "error".into(),
            error: Some(format!("no review report at {}: {e}", path.display())),
            ..Default::default()
        },
    }
}

const COMMIT_PROMPT: &str = "Commit your work now. Stage only the files that belong to this change (no build output, caches or scratch files), commit with a message that describes the whole change, and reply with the commit and any files you left uncommitted and why.";

fn fix_prompt(r: &ReviewOut, plan: Option<&Path>, report: &Path) -> String {
    let plan_uri = plan.map(|p| p.display().to_string());
    let report_uri = report.display().to_string();
    let blocking: Vec<&review::Finding> = r.findings.iter().filter(|f| f.blocking).map(|f| &f.finding).collect();
    let picked = if blocking.is_empty() { r.findings.iter().map(|f| &f.finding).collect() } else { blocking };
    if picked.is_empty() {
        review::raw_message(&format!("Reviewer summary: {}", r.summary), plan_uri.as_deref(), &report_uri)
    } else {
        review::fix_message(&picked, plan_uri.as_deref(), &report_uri, None)
    }
}

pub(crate) struct Job {
    pub dir: PathBuf,
    pub plan: Option<String>,
    pub new_worktree: Option<String>,
    pub max_fix: u32,
    pub notify: Option<PathBuf>,
}

fn fail_with(rec: &mut Record, result: Outcome, e: String) {
    rec.result = result;
    rec.error = Some(e);
}

async fn body<S: Steps>(steps: &mut S, job: &Job, rec: &mut Record) {
    let dir = &job.dir;
    if let Some(branch) = &job.new_worktree {
        let _ = review::git(&rec.repo, &["fetch", "-q"], None).await;
        let base = match review::git(&rec.repo, &["rev-parse", "--verify", "-q", "origin/HEAD"], None).await {
            Ok(s) if !s.trim().is_empty() => s,
            _ => match review::git(&rec.repo, &["rev-parse", "HEAD"], None).await {
                Ok(s) => s,
                Err(e) => return fail_with(rec, Outcome::Error, e),
            },
        };
        let base = base.trim().to_string();
        let wt = rec.worktree.display().to_string();
        eprintln!("== worktree {wt} on {branch}");
        if let Err(e) = review::git(&rec.repo, &["worktree", "add", "-q", "-b", branch, &wt, &base], None).await {
            return fail_with(rec, Outcome::Error, e);
        }
        rec.base = base;
        rec.created_worktree = true;
        save(dir, rec);
    }
    if let Some(id) = &job.plan {
        eprintln!("== implement: mu plan approve {id}");
        let out = dir.join("implement.md");
        let (session, res) = steps.implement(&rec.worktree, id, &out).await;
        rec.session = session;
        rec.last_report = Some(out);
        save(dir, rec);
        if let Err(e) = res {
            return fail_with(rec, Outcome::Failed, e);
        }
    }
    let Some(session) = rec.session.clone() else {
        return fail_with(rec, Outcome::Failed, "no implementer session".into());
    };
    let mut fixed = 0u32;
    loop {
        let n = rec.reviews + 1;
        let rep = dir.join(format!("review-{n}.json"));
        eprintln!("== review {n}");
        steps.review(&rec.worktree, &rec.base, rec.plan_path.as_deref(), &rep).await;
        let v = read_review(&rep);
        rec.reviews = n;
        rec.verdict = Some(v.verdict.clone());
        rec.last_review = Some(rep.clone());
        save(dir, rec);
        if !(v.verdict == "fail" && fixed < job.max_fix) {
            break;
        }
        fixed += 1;
        rec.fixes += 1;
        let k = rec.fixes;
        eprintln!("== fix {k}");
        let out = dir.join(format!("fix-{k}.md"));
        let prompt = fix_prompt(&v, rec.plan_path.as_deref(), &rep);
        if let Err(e) = steps.resume(&rec.worktree, &session, &prompt, &out).await {
            return fail_with(rec, Outcome::Failed, e);
        }
        rec.last_report = Some(out);
        save(dir, rec);
    }
    if review::git(&rec.worktree, &["status", "--porcelain"], None).await.is_ok_and(|s| !s.trim().is_empty()) {
        eprintln!("== commit");
        if let Err(e) = steps.resume(&rec.worktree, &session, COMMIT_PROMPT, &dir.join("commit.md")).await {
            eprintln!("mu deliver: commit step: {e}");
        }
    }
    rec.result = match rec.verdict.as_deref() {
        Some("pass" | "nothing") => Outcome::Pass,
        Some("fail") => Outcome::Fail,
        _ => Outcome::Error,
    };
}

pub(crate) async fn run_loop<S: Steps>(steps: &mut S, job: &Job, rec: &mut Record) {
    rec.result = Outcome::Running;
    rec.pid = std::process::id();
    rec.started = mu_common::time::iso_ms();
    rec.finished = None;
    rec.error = None;
    rec.notified = None;
    rec.max_fix = job.max_fix;
    save(&job.dir, rec);

    body(steps, job, rec).await;

    let dir = &job.dir;
    rec.leftovers = review::git(&rec.worktree, &["status", "--porcelain"], None)
        .await
        .map(|s| s.trim_end_matches('\n').lines().filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    if let Ok(b) = review::git(&rec.worktree, &["branch", "--show-current"], None).await {
        rec.branch = b.trim().to_string();
    }
    rec.head = review::git(&rec.worktree, &["rev-parse", "HEAD"], None).await.ok().map(|s| s.trim().to_string());
    rec.finished = Some(mu_common::time::iso_ms());
    save(dir, rec);
    if let Err(e) = std::fs::write(dir.join("summary.md"), summary(rec)) {
        eprintln!("mu deliver: cannot write {}: {e}", dir.join("summary.md").display());
    }
    if let Some(s) = &job.notify {
        let text = format!(
            "[mu deliver finished] delivery {}: {}\nRecord: {}/delivery.json (summary.md, log). Worktree: {}",
            rec.id,
            rec.result.name(),
            dir.display(),
            rec.worktree.display()
        );
        rec.notified = Some(steps.notify(&rec.worktree, s, &text).await);
        save(dir, rec);
    }
}

fn summary(r: &Record) -> String {
    use std::fmt::Write;
    let short = |s: &str| s[..s.len().min(7)].to_string();
    let mut o = String::new();
    let _ = writeln!(o, "# {}\n", if r.title.is_empty() { &r.id } else { &r.title });
    let _ = writeln!(
        o,
        "- result: {} (review verdict `{}`, {} review(s), {} fix round(s))",
        r.result.name(),
        r.verdict.as_deref().unwrap_or("none"),
        r.reviews,
        r.fixes
    );
    let _ = writeln!(
        o,
        "- branch: `{}` at `{}`, worktree `{}`, base `{}`",
        r.branch,
        short(r.head.as_deref().unwrap_or("")),
        r.worktree.display(),
        short(&r.base)
    );
    let _ = writeln!(o, "- session: `{}`", r.session.as_ref().map_or("none".into(), |s| s.display().to_string()));
    if let Some(e) = &r.error {
        let _ = writeln!(o, "- error: {e}");
    }
    let _ = writeln!(o, "\n## Last review\n");
    match &r.last_review {
        None => {
            let _ = writeln!(o, "no report");
        }
        Some(p) => {
            let v = read_review(p);
            let text = if !v.summary.trim().is_empty() {
                v.summary.trim().to_string()
            } else {
                v.error.clone().unwrap_or_else(|| "no summary".into())
            };
            let _ = writeln!(o, "{text}");
            for f in &v.findings {
                let _ = writeln!(
                    o,
                    "- [P{}{}] {} ({})",
                    f.finding.priority,
                    if f.blocking { ", blocking" } else { "" },
                    f.finding.title,
                    review::loc(&f.finding)
                );
            }
        }
    }
    if !r.leftovers.is_empty() {
        let _ = writeln!(o, "\n## Left uncommitted\n\n```\n{}\n```", r.leftovers.join("\n"));
    }
    let report = r.last_report.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
    let _ = writeln!(o, "\n## Implementer's last report\n\n{}", report.as_deref().unwrap_or("none").trim_end());
    o
}

fn try_lock(path: &Path) -> Result<Option<File>, String> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("cannot create {}: {e}", p.display()))?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    // SAFETY: plain flock on a descriptor we own.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(f));
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(None)
    } else {
        Err(format!("cannot lock {}: {e}", path.display()))
    }
}

fn running_error(id: &str, dir: &Path) -> String {
    let pid = load(dir).map(|r| format!(" (pid {})", r.pid)).unwrap_or_default();
    format!("delivery '{id}' is already running{pid}: {}", dir.display())
}

async fn current_branch(top: &Path) -> String {
    review::git(top, &["branch", "--show-current"], None).await.map(|s| s.trim().to_string()).unwrap_or_default()
}

async fn prepare(a: &DeliverArgs, cwd: &Path) -> Result<(Job, Record), String> {
    let top = review::repo_root(cwd).await.map_err(|_| "mu deliver needs a git repository".to_string())?;
    let notify = match &a.notify {
        Some(p) => Some(session::find(cwd, p)?),
        None => std::env::var_os("PI_SESSION_FILE")
            .filter(|v| !v.is_empty())
            .and_then(|v| session::find(cwd, Path::new(&v)).ok()),
    };
    let ddir = deliveries_dir(cwd).await;
    let mut job = Job { dir: PathBuf::new(), plan: None, new_worktree: None, max_fix: a.max_fix, notify };
    let rec = match (&a.id, a.loop_only) {
        (Some(id), false) => {
            let store = plans::store_for(cwd).await;
            let (meta, _) = store.load_pending(id)?;
            let drift = plans::drift(&meta, cwd).await;
            if matches!(drift, plans::Drift::Changed { .. }) {
                return Err(format!(
                    "plan '{id}' is stale ({}); refine it first: mu plan refine {id}",
                    drift.describe()
                ));
            }
            let dir = ddir.join(id);
            if dir.join("delivery.json").exists() {
                return Err(format!(
                    "delivery '{id}' already exists: {}; restart it with mu deliver --loop {id}",
                    dir.display()
                ));
            }
            let (worktree, base, branch);
            if a.worktree {
                let name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let wt = top.parent().unwrap_or(&top).join(format!("{name}-{id}"));
                if wt.exists() {
                    return Err(format!("{} already exists", wt.display()));
                }
                let b = format!("plan/{id}");
                if review::git(&top, &["rev-parse", "-q", "--verify", &format!("refs/heads/{b}")], None).await.is_ok() {
                    return Err(format!("branch {b} already exists"));
                }
                worktree = wt;
                base = String::new();
                job.new_worktree = Some(b.clone());
                branch = b;
            } else {
                if review::git(&top, &["status", "--porcelain"], None).await.is_ok_and(|s| !s.trim().is_empty()) {
                    return Err(
                        "the working tree has uncommitted changes; commit or stash them, or use --worktree".into()
                    );
                }
                base = review::git(&top, &["rev-parse", "HEAD"], None)
                    .await
                    .map(|s| s.trim().to_string())
                    .map_err(|e| format!("cannot resolve HEAD: {e}"))?;
                let cur = current_branch(&top).await;
                branch = if cur.is_empty() { format!("plan/{id}") } else { cur };
                worktree = top.clone();
            }
            job.plan = Some(id.clone());
            job.dir = dir;
            Record {
                id: id.clone(),
                plan: Some(id.clone()),
                plan_path: Some(store.md_path(id)),
                title: meta.title,
                repo: top.clone(),
                worktree,
                branch,
                base,
                ..Default::default()
            }
        }
        (Some(id), true) => {
            let dir = ddir.join(id);
            let r = load(&dir).map_err(|_| format!("no delivery '{id}' in {}", ddir.display()))?;
            if r.session.is_none() {
                return Err(format!("delivery '{id}' has no implementer session to resume"));
            }
            if !r.worktree.is_dir() {
                return Err(format!("{} no longer exists", r.worktree.display()));
            }
            job.dir = dir;
            r
        }
        (None, _) => {
            let (Some(s), Some(b)) = (&a.session, &a.base) else {
                return Err("--loop without a delivery id needs --session and --base".into());
            };
            let session = session::find(cwd, s)?;
            let session = std::fs::canonicalize(&session).unwrap_or(session);
            let base = review::git(&top, &["rev-parse", "--verify", "--quiet", &format!("{b}^{{commit}}")], None)
                .await
                .map(|s| s.trim().to_string())
                .map_err(|_| format!("unknown base {b}"))?;
            let plan_path = match &a.plan {
                Some(p) => Some(
                    std::fs::canonicalize(cwd.join(p))
                        .map_err(|e| format!("cannot read --plan {}: {e}", p.display()))?,
                ),
                None => None,
            };
            let id = session::id_of(&session);
            let dir = ddir.join(&id);
            let mut r = if dir.join("delivery.json").exists() {
                load(&dir)?
            } else {
                let br = current_branch(&top).await;
                Record {
                    id: id.clone(),
                    title: if br.is_empty() { id.clone() } else { br },
                    repo: top.clone(),
                    ..Default::default()
                }
            };
            r.session = Some(session);
            r.base = base;
            r.plan_path = plan_path;
            r.worktree = top.clone();
            job.dir = dir;
            r
        }
    };
    match try_lock(&job.dir.join("lock"))? {
        Some(_) => {}
        None => return Err(running_error(&rec.id, &job.dir)),
    }
    Ok((job, rec))
}

pub fn run_cli(a: DeliverArgs, cwd: &Path) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mu deliver: {e}");
            return 2;
        }
    };
    let local = tokio::task::LocalSet::new();
    let (job, mut rec) = match local.block_on(&rt, prepare(&a, cwd)) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("mu deliver: {e}");
            return 2;
        }
    };
    let exe = match self_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("mu deliver: cannot find the mu binary: {e}");
            return 2;
        }
    };
    if a.detach {
        return detach(&exe, &job.dir, cwd);
    }
    let _lock = match try_lock(&job.dir.join("lock")) {
        Ok(Some(f)) => f,
        Ok(None) => {
            eprintln!("mu deliver: {}", running_error(&rec.id, &job.dir));
            return 2;
        }
        Err(e) => {
            eprintln!("mu deliver: {e}");
            return 2;
        }
    };
    let mut steps = MuSteps { cmd: vec![exe.into()] };
    local.block_on(&rt, run_loop(&mut steps, &job, &mut rec));
    if let Ok(j) = serde_json::to_string_pretty(&rec) {
        println!("{j}");
    }
    eprintln!("mu deliver: {}: {} · {}/delivery.json", rec.id, rec.result.name(), job.dir.display());
    match rec.result {
        Outcome::Pass => 0,
        Outcome::Fail => 1,
        _ => 2,
    }
}

fn detach(exe: &Path, dir: &Path, cwd: &Path) -> i32 {
    let log = dir.join("log");
    let opened = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::OpenOptions::new().create(true).append(true).open(&log))
        .and_then(|f| f.try_clone().map(|g| (f, g)));
    let (out, err) = match opened {
        Ok(x) => x,
        Err(e) => {
            eprintln!("mu deliver: cannot open {}: {e}", log.display());
            return 2;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1).filter(|a| a != "--detach"))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    // SAFETY: only setsid between fork and exec.
    unsafe {
        cmd.pre_exec(|| if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mu deliver: cannot start: {e}");
            return 2;
        }
    };
    std::thread::sleep(std::time::Duration::from_secs(1));
    if let Ok(Some(_)) = child.try_wait() {
        eprintln!("mu deliver: exited at once; see {}", log.display());
        if let Ok(t) = std::fs::read_to_string(&log) {
            let lines: Vec<&str> = t.lines().collect();
            for l in &lines[lines.len().saturating_sub(5)..] {
                eprintln!("{l}");
            }
        }
        return 2;
    }
    println!("started: pid {} · log {} · record {}/delivery.json", child.id(), log.display(), dir.display());
    0
}

/// The `DeliverFn` the binary passes to the agent: `/deliver` starts `mu deliver` through it.
pub fn detached_starter() -> DeliverFn {
    Rc::new(|id: String, session: PathBuf, cwd: PathBuf| {
        Box::pin(start_detached(id, session, cwd)) as Pin<Box<dyn std::future::Future<Output = Result<String, String>>>>
    })
}

/// Starts `mu deliver <id> --worktree --detach --notify` for `session`, detached. `Ok` is its stdout.
async fn start_detached(id: String, session: PathBuf, cwd: PathBuf) -> Result<String, String> {
    let exe = self_exe().map_err(|e| format!("cannot find the mu binary: {e}"))?;
    let out = tokio::process::Command::new(exe)
        .args(["deliver", &id, "--worktree", "--detach", "--notify"])
        .arg(session)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(format!("cannot start mu deliver: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::collections::VecDeque;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-deliver-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn repo() -> PathBuf {
        let d = tmp();
        sh(&d, &["init", "-q"]);
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        sh(&d, &["add", "."]);
        sh(&d, &["commit", "-qm", "init"]);
        d
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[derive(Default)]
    struct Fake {
        reviews: VecDeque<Value>,
        implemented: u32,
        review_outs: Vec<PathBuf>,
        resumes: Vec<(PathBuf, String)>,
        resume_outs: Vec<PathBuf>,
        notes: Vec<(PathBuf, String)>,
        impl_result: Option<(Option<PathBuf>, Result<(), String>)>,
    }

    impl Steps for Fake {
        async fn implement(&mut self, _: &Path, _: &str, out: &Path) -> (Option<PathBuf>, Result<(), String>) {
            self.implemented += 1;
            std::fs::write(out, "implemented").unwrap();
            self.impl_result.clone().unwrap_or((Some("/s/impl.jsonl".into()), Ok(())))
        }
        async fn review(&mut self, _: &Path, _: &str, _: Option<&Path>, out: &Path) {
            self.review_outs.push(out.to_path_buf());
            if let Some(v) = self.reviews.pop_front() {
                std::fs::write(out, v.to_string()).unwrap();
            }
        }
        async fn resume(&mut self, _: &Path, s: &Path, prompt: &str, out: &Path) -> Result<(), String> {
            self.resumes.push((s.to_path_buf(), prompt.to_string()));
            self.resume_outs.push(out.to_path_buf());
            std::fs::write(out, "fixed").unwrap();
            Ok(())
        }
        async fn notify(&mut self, _: &Path, s: &Path, text: &str) -> String {
            self.notes.push((s.to_path_buf(), text.to_string()));
            "delivered (pid 1)".into()
        }
    }

    fn failing() -> Value {
        json!({"verdict":"fail","summary":"s","findings":[
            {"title":"Off by one","body":"b","file":"a.txt","line":1,"priority":1,"confidence":0.9,"blocking":true},
            {"title":"nit","body":"n","file":"a.txt","priority":3,"confidence":1.0,"blocking":false}]})
    }

    fn passing() -> Value {
        json!({"verdict":"pass","summary":"ok","findings":[]})
    }

    fn setup(plan: bool) -> (Job, Record, PathBuf) {
        crate::testutil::home();
        let repo = repo();
        let plan_file = tmp().join("plan.md");
        std::fs::write(&plan_file, "# plan\n").unwrap();
        let base = sh(&repo, &["rev-parse", "HEAD"]).trim().to_string();
        let job = Job {
            dir: tmp().join("rec"),
            plan: plan.then(|| "x".to_string()),
            new_worktree: None,
            max_fix: 2,
            notify: None,
        };
        let rec = Record {
            id: "x".into(),
            repo: repo.clone(),
            worktree: repo.clone(),
            base,
            plan_path: Some(plan_file),
            ..Default::default()
        };
        (job, rec, repo)
    }

    fn go(f: &mut Fake, job: &Job, rec: &mut Record) {
        rt().block_on(run_loop(f, job, rec));
    }

    #[test]
    fn blocking_findings_go_back_to_the_implementer() {
        let (job, mut rec, _) = setup(true);
        let mut f = Fake { reviews: [failing(), passing()].into(), ..Default::default() };
        go(&mut f, &job, &mut rec);
        assert_eq!(f.resumes.len(), 1);
        let (s, p) = &f.resumes[0];
        assert_eq!(s, Path::new("/s/impl.jsonl"));
        assert!(p.starts_with("A reviewer checked your implementation against the plan at"), "{p}");
        assert!(p.contains("1. [P1] Off by one (a.txt:1)"), "{p}");
        assert!(!p.contains("nit"));
        assert_eq!((rec.result, rec.reviews, rec.fixes), (Outcome::Pass, 2, 1));
        assert_eq!(rec.session.as_deref(), Some(Path::new("/s/impl.jsonl")));
        for n in ["review-1.json", "review-2.json", "fix-1.md", "summary.md", "delivery.json"] {
            assert!(job.dir.join(n).exists(), "{n}");
        }
    }

    #[test]
    fn stops_after_max_fix_with_fail() {
        let (job, mut rec, _) = setup(true);
        let mut f = Fake { reviews: [failing(), failing(), failing()].into(), ..Default::default() };
        go(&mut f, &job, &mut rec);
        assert_eq!(f.review_outs.len(), 3);
        assert_eq!(f.resumes.len(), 2);
        assert!(f.resumes.iter().all(|(_, p)| p.contains("Off by one")));
        assert_eq!((rec.result, rec.fixes), (Outcome::Fail, 2));
        assert!(rec.last_review.unwrap().ends_with("review-3.json"));
    }

    #[test]
    fn restart_runs_review_and_fix_only() {
        let (job, mut rec, _) = setup(false);
        rec.session = Some("/s/old.jsonl".into());
        rec.reviews = 1;
        rec.fixes = 1;
        let mut f = Fake { reviews: [failing(), passing()].into(), ..Default::default() };
        go(&mut f, &job, &mut rec);
        assert_eq!(f.implemented, 0);
        assert!(f.review_outs[0].ends_with("review-2.json"));
        assert_eq!(f.resumes[0].0, Path::new("/s/old.jsonl"));
        assert!(f.resume_outs[0].ends_with("fix-2.md"));
        assert_eq!((rec.reviews, rec.fixes, rec.result), (3, 2, Outcome::Pass));
    }

    #[test]
    fn failed_implement_keeps_the_session() {
        let (job, mut rec, _) = setup(true);
        let s = PathBuf::from("/s/impl.jsonl");
        let mut f = Fake { impl_result: Some((Some(s.clone()), Err("mu plan exited 3".into()))), ..Default::default() };
        go(&mut f, &job, &mut rec);
        assert_eq!(rec.result, Outcome::Failed);
        assert_eq!(rec.session, Some(s));
        assert!(rec.error.unwrap().contains("exited 3"));
        assert!(f.review_outs.is_empty());
    }

    #[test]
    fn commit_step_and_leftovers() {
        let (job, mut rec, repo) = setup(true);
        std::fs::write(repo.join("new.txt"), "x").unwrap();
        let mut f = Fake { reviews: [passing()].into(), ..Default::default() };
        go(&mut f, &job, &mut rec);
        assert_eq!(f.resumes.last().unwrap().1, COMMIT_PROMPT);
        assert_eq!(rec.leftovers, vec!["?? new.txt".to_string()]);
    }

    #[test]
    fn notifies_the_starting_session() {
        let (mut job, mut rec, _) = setup(true);
        job.notify = Some("/s/boss.jsonl".into());
        let mut f = Fake { reviews: [passing()].into(), ..Default::default() };
        go(&mut f, &job, &mut rec);
        assert_eq!(f.notes.len(), 1);
        assert_eq!(f.notes[0].0, Path::new("/s/boss.jsonl"));
        assert!(f.notes[0].1.starts_with("[mu deliver finished] delivery x: pass"), "{}", f.notes[0].1);
        assert_eq!(rec.notified.as_deref(), Some("delivered (pid 1)"));
    }

    #[test]
    fn missing_report_is_error() {
        let (job, mut rec, _) = setup(true);
        let mut f = Fake::default();
        go(&mut f, &job, &mut rec);
        assert_eq!(rec.verdict.as_deref(), Some("error"));
        assert_eq!(rec.result, Outcome::Error);
        assert!(f.resumes.is_empty());
    }

    #[test]
    fn worktree_is_created_from_head_without_upstream() {
        let (mut job, mut rec, repo) = setup(true);
        let wt = tmp().join("repo-x");
        job.new_worktree = Some("plan/x".into());
        rec.worktree = wt.clone();
        let mut f = Fake { reviews: [passing()].into(), ..Default::default() };
        go(&mut f, &job, &mut rec);
        assert!(wt.is_dir());
        assert_eq!(sh(&wt, &["branch", "--show-current"]).trim(), "plan/x");
        assert_eq!(rec.base, sh(&repo, &["rev-parse", "HEAD"]).trim());
        assert!(rec.created_worktree);
    }

    #[test]
    fn mu_steps_argv() {
        crate::testutil::home();
        let repo = repo();
        let script = tmp().join("fake.sh");
        std::fs::write(
            &script,
            "printf '%s|' \"$@\" >> \"$PWD/args.log\"; echo >> \"$PWD/args.log\"\n\
             if [ \"$1\" = review ]; then while [ $# -gt 0 ]; do [ \"$1\" = --out ] && printf '{\"verdict\":\"pass\"}' > \"$2\"; shift; done; fi\n\
             echo final answer\n",
        )
        .unwrap();
        let mut steps = MuSteps { cmd: vec!["sh".into(), script.into()] };
        let out = tmp();
        rt().block_on(async {
            let store = plans::store_for(&repo).await;
            store
                .save(plans::Save {
                    slug: "x",
                    title: "T",
                    content: "# plan\n",
                    read: vec![],
                    session: None,
                    base: None,
                    adopt: None,
                })
                .unwrap();
            store.approve("x", "keep", Some(Path::new("/s/impl.jsonl"))).unwrap();
            let (s, r) = steps.implement(&repo, "x", &out.join("i.md")).await;
            assert!(r.is_ok());
            assert_eq!(s.as_deref(), Some(Path::new("/s/impl.jsonl")));
            steps.review(&repo, "B", Some(Path::new("/p.md")), &out.join("r.json")).await;
            steps.resume(&repo, Path::new("/s/impl.jsonl"), "do it", &out.join("f.md")).await.unwrap();
        });
        let log = std::fs::read_to_string(repo.join("args.log")).unwrap();
        let o = out.join("r.json");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "plan|approve|x|");
        assert_eq!(lines[1], format!("review|--base|B|--plan|/p.md|--out|{}|", o.display()));
        assert_eq!(lines[2], "--resume|/s/impl.jsonl|-p|do it|");
        assert_eq!(std::fs::read_to_string(out.join("f.md")).unwrap().trim(), "final answer");
    }

    fn args(id: Option<&str>) -> DeliverArgs {
        DeliverArgs {
            id: id.map(String::from),
            worktree: false,
            loop_only: false,
            session: None,
            base: None,
            plan: None,
            max_fix: 2,
            notify: None,
            detach: false,
        }
    }

    fn park(repo: &Path, slug: &str) -> plans::Store {
        rt().block_on(async {
            let store = plans::store_for(repo).await;
            store
                .save(plans::Save {
                    slug,
                    title: "T",
                    content: "# plan\n",
                    read: vec![],
                    session: None,
                    base: None,
                    adopt: None,
                })
                .unwrap();
            store
        })
    }

    fn refusal(a: &DeliverArgs, cwd: &Path) -> String {
        match rt().block_on(prepare(a, cwd)) {
            Ok(_) => panic!("expected a refusal"),
            Err(e) => e,
        }
    }

    #[test]
    fn prepare_refusals() {
        crate::testutil::home();
        let repo = repo();
        assert!(refusal(&args(Some("nope")), &repo).starts_with("plan 'nope' not found in "));

        let store = park(&repo, "ap");
        store.approve("ap", "keep", None).unwrap();
        assert!(refusal(&args(Some("ap")), &repo).contains("is approved, not pending"));

        park(&repo, "y");
        std::fs::write(repo.join("dirty.txt"), "x").unwrap();
        assert!(refusal(&args(Some("y")), &repo).starts_with("the working tree has uncommitted changes; "));
        std::fs::remove_file(repo.join("dirty.txt")).unwrap();

        let mut l = args(None);
        l.loop_only = true;
        assert_eq!(refusal(&l, &repo), "--loop without a delivery id needs --session and --base");

        park(&repo, "z");
        let dir = rt().block_on(deliveries_dir(&repo)).join("z");
        let _held = try_lock(&dir.join("lock")).unwrap().unwrap();
        assert!(refusal(&args(Some("z")), &repo).starts_with("delivery 'z' is already running"));
    }
}
