//! The delivery loop: implement, then review and fix until the review passes or the fix rounds run
//! out, commit, and write the record.

use super::record::{Outcome, Record, save, summary};
use super::steps::{COMMIT_PROMPT, Steps, fix_prompt, read_review};
use mu_base::git::git;
use std::path::PathBuf;

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
        let _ = git(&rec.repo, &["fetch", "-q"], None).await;
        let base = match git(&rec.repo, &["rev-parse", "--verify", "-q", "origin/HEAD"], None).await {
            Ok(s) if !s.trim().is_empty() => s,
            _ => match git(&rec.repo, &["rev-parse", "HEAD"], None).await {
                Ok(s) => s,
                Err(e) => return fail_with(rec, Outcome::Error, e.to_string()),
            },
        };
        let base = base.trim().to_string();
        let wt = rec.worktree.display().to_string();
        eprintln!("== worktree {wt} on {branch}");
        if let Err(e) = git(&rec.repo, &["worktree", "add", "-q", "-b", branch, &wt, &base], None).await {
            return fail_with(rec, Outcome::Error, e.to_string());
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
    if git(&rec.worktree, &["status", "--porcelain"], None).await.is_ok_and(|s| !s.trim().is_empty()) {
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
    rec.leftovers = git(&rec.worktree, &["status", "--porcelain"], None)
        .await
        .map(|s| s.trim_end_matches('\n').lines().filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    if let Ok(b) = git(&rec.worktree, &["branch", "--show-current"], None).await {
        rec.branch = b.trim().to_string();
    }
    rec.head = git(&rec.worktree, &["rev-parse", "HEAD"], None).await.ok().map(|s| s.trim().to_string());
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
