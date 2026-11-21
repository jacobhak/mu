//! `mu deliver`: approve and implement a parked plan, review it against the plan, send blocking findings back to the implementer session, then have it commit.

mod engine;
mod record;
mod steps;

use engine::{Job, run_loop};
use mu_agent::agent::DeliverFn;
use mu_agent::plans;
use mu_agent::review::facts::repo_root;
use mu_base::git::git;
use mu_common::paths::self_exe;
use mu_session::session::list;
pub use record::{Outcome, Record};
use record::{deliveries_dir, load};
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use steps::MuSteps;

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
    match mu_base::process::try_flock(&f, libc::LOCK_EX) {
        Ok(()) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

fn running_error(id: &str, dir: &Path) -> String {
    let pid = load(dir).map(|r| format!(" (pid {})", r.pid)).unwrap_or_default();
    format!("delivery '{id}' is already running{pid}: {}", dir.display())
}

async fn current_branch(top: &Path) -> String {
    git(top, &["branch", "--show-current"], None).await.map(|s| s.trim().to_string()).unwrap_or_default()
}

async fn prepare(a: &DeliverArgs, cwd: &Path) -> Result<(Job, Record), String> {
    let top = repo_root(cwd).await.map_err(|_| "mu deliver needs a git repository".to_string())?;
    let notify = match &a.notify {
        Some(p) => Some(list::find(cwd, p).map_err(|e| e.to_string())?),
        None => std::env::var_os("PI_SESSION_FILE")
            .filter(|v| !v.is_empty())
            .and_then(|v| list::find(cwd, Path::new(&v)).ok()),
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
                if git(&top, &["rev-parse", "-q", "--verify", &format!("refs/heads/{b}")], None).await.is_ok() {
                    return Err(format!("branch {b} already exists"));
                }
                worktree = wt;
                base = String::new();
                job.new_worktree = Some(b.clone());
                branch = b;
            } else {
                if git(&top, &["status", "--porcelain"], None).await.is_ok_and(|s| !s.trim().is_empty()) {
                    return Err(
                        "the working tree has uncommitted changes; commit or stash them, or use --worktree".into()
                    );
                }
                base = git(&top, &["rev-parse", "HEAD"], None)
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
            let session = list::find(cwd, s).map_err(|e| e.to_string())?;
            let session = std::fs::canonicalize(&session).unwrap_or(session);
            let base = git(&top, &["rev-parse", "--verify", "--quiet", &format!("{b}^{{commit}}")], None)
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
            let id = list::id_of(&session);
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
    let rt = match mu_base::runtime::current_thread_runtime() {
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
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(mu_base::process::new_session);
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
    use super::steps::{COMMIT_PROMPT, Steps};
    use super::*;
    use mu_testutil::{repo, rt, sh, tmp};
    use serde_json::{Value, json};
    use std::collections::VecDeque;

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

    /// The repo `setup` made, which derefs to its path, with the other directories it made; all are removed on drop.
    struct Dirs {
        repo: mu_testutil::TempDir,
        _plan: mu_testutil::TempDir,
        _rec: mu_testutil::TempDir,
    }

    impl std::ops::Deref for Dirs {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.repo
        }
    }

    fn setup(plan: bool) -> (Job, Record, Dirs) {
        mu_testutil::home();
        let repo = repo();
        let plan_dir = tmp();
        let plan_file = plan_dir.join("plan.md");
        std::fs::write(&plan_file, "# plan\n").unwrap();
        let base = sh(&repo, &["rev-parse", "HEAD"]).trim().to_string();
        let rec_dir = tmp();
        let job = Job {
            dir: rec_dir.join("rec"),
            plan: plan.then(|| "x".to_string()),
            new_worktree: None,
            max_fix: 2,
            notify: None,
        };
        let rec = Record {
            id: "x".into(),
            repo: repo.to_path_buf(),
            worktree: repo.to_path_buf(),
            base,
            plan_path: Some(plan_file),
            ..Default::default()
        };
        (job, rec, Dirs { repo, _plan: plan_dir, _rec: rec_dir })
    }

    fn go(f: &mut Fake, job: &Job, rec: &mut Record) {
        rt().block_on(run_loop(f, job, rec));
    }

    #[test]
    fn blocking_findings_go_back_to_the_implementer() {
        let (job, mut rec, _dirs) = setup(true);
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
        let (job, mut rec, _dirs) = setup(true);
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
        let (job, mut rec, _dirs) = setup(false);
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
        let (job, mut rec, _dirs) = setup(true);
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
        let (mut job, mut rec, _dirs) = setup(true);
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
        let (job, mut rec, _dirs) = setup(true);
        let mut f = Fake::default();
        go(&mut f, &job, &mut rec);
        assert_eq!(rec.verdict.as_deref(), Some("error"));
        assert_eq!(rec.result, Outcome::Error);
        assert!(f.resumes.is_empty());
    }

    #[test]
    fn worktree_is_created_from_head_without_upstream() {
        let (mut job, mut rec, repo) = setup(true);
        let wt_dir = tmp();
        let wt = wt_dir.join("repo-x");
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
        mu_testutil::home();
        let repo = repo();
        let script_dir = tmp();
        let script = script_dir.join("fake.sh");
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
        // Every subprocess runs unprompted: approving the plan for delivery is the authorization.
        assert_eq!(lines[0], "plan|approve|x|--permissions|yolo|");
        assert_eq!(lines[1], format!("review|--base|B|--plan|/p.md|--out|{}|--permissions|yolo|", o.display()));
        assert_eq!(lines[2], "--resume|/s/impl.jsonl|-p|do it|--permissions|yolo|");
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
        mu_testutil::home();
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
