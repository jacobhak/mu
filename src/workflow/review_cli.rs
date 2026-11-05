//! `mu review`: the `/review` pipeline without a session. It gathers the same facts and request as
//! the in-session review (`mu_agent::review`), runs `review-lead` and its workers as child processes, and
//! prints the verdict as JSON or markdown.
use mu_agent::review::facts::{Base, Worktree, repo_root};
use mu_agent::review::report::{
    Finding, Meta, PlanStep, Report, is_blocking, render_markdown, report_of, report_schema,
};
use mu_base::git::short_sha;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Options for `mu review`, the headless `/review`.
pub struct CliOpts {
    /// A git ref to diff against. `HEAD` reviews uncommitted changes.
    pub base: String,
    /// The approved plan the changes should match.
    pub plan: Option<PathBuf>,
    pub focus: Option<String>,
    /// Write the output here instead of stdout.
    pub out: Option<PathBuf>,
    /// Markdown instead of JSON.
    pub markdown: bool,
}

/// What `mu review` prints. `verdict` is `pass`, `fail` (fixes needed), `nothing` (no reviewable
/// changes) or `error` (the review did not complete; see `error`).
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliVerdict {
    pub verdict: &'static str,
    pub error: Option<String>,
    pub base: String,
    pub current: String,
    pub files: usize,
    pub added: u32,
    pub removed: u32,
    pub model: String,
    pub summary: String,
    pub plan: Vec<PlanStep>,
    pub findings: Vec<CliFinding>,
    pub coverage: String,
    pub dropped: u32,
    /// The reviewer's text when it returned no JSON report.
    pub raw_report: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CliFinding {
    #[serde(flatten)]
    pub finding: Finding,
    /// Same rule as the `/review` findings list: P1 or P2 with confidence of at least 0.6.
    pub blocking: bool,
}

/// Process exit code for a verdict: 0 pass or nothing to review, 1 fixes needed, 2 error.
pub fn exit_code(verdict: &str) -> i32 {
    match verdict {
        "pass" | "nothing" => 0,
        "fail" => 1,
        _ => 2,
    }
}

/// The `/review` pipeline without a session: the same facts, request, `review-lead` and workers.
/// Returns the verdict and the parsed report (absent unless the reviewer returned one).
async fn review_once(o: &CliOpts, cwd: &Path) -> (CliVerdict, Option<Report>) {
    let mut v = CliVerdict { verdict: "error", ..Default::default() };
    match review_steps(o, cwd, &mut v).await {
        Ok(report) => (v, report),
        Err(e) => {
            v.error = Some(e);
            (v, None)
        }
    }
}

async fn review_steps(o: &CliOpts, cwd: &Path, v: &mut CliVerdict) -> Result<Option<Report>, String> {
    let wt = Worktree::snapshot(repo_root(cwd).await?).await?;
    let base = if o.base == "HEAD" { Base::Head { label: "HEAD" } } else { Base::Rev(&o.base) };
    let facts = wt.gather(base).await?;
    v.base = facts.base.clone();
    v.current = facts.cur.clone();
    v.files = facts.files.len();
    v.added = facts.added;
    v.removed = facts.removed;
    if facts.files.is_empty() {
        v.verdict = "nothing";
        v.summary = format!("nothing to review: no reviewable changes since {}", short_sha(&facts.base));
        return Ok(None);
    }
    let plan = match &o.plan {
        Some(p) => {
            Some(std::fs::canonicalize(cwd.join(p)).map_err(|e| format!("cannot read --plan {}: {e}", p.display()))?)
        }
        None => None,
    };
    let request = facts.request(plan, &[], o.focus.as_deref());
    let cfgs = mu_base::config::Configs::load();
    v.model = mu_base::models::role_selector("review", &cfgs).unwrap_or_else(|| "default".into());
    let exe = mu_common::paths::self_exe().map_err(|e| format!("cannot locate mu executable: {e}"))?;
    let payload =
        serde_json::json!({"task": request, "agent": mu_tools::context::REVIEW_LEAD, "outputSchema": report_schema()});
    let cancel = tokio_util::sync::CancellationToken::new();
    let out = mu_tools::tools::subagent::run_child(&exe, &facts.root, &payload, &cancel, |name, summary| {
        eprintln!("  {name} {summary}");
    })
    .await;
    if !out.ok {
        return Err(out.text);
    }
    let Some(report) = report_of(&out) else {
        v.raw_report = Some(out.text);
        return Err("reviewer returned an unstructured report".into());
    };
    v.verdict = if report.verdict == "pass" { "pass" } else { "fail" };
    v.summary = report.summary.trim().to_string();
    v.plan = report.plan.clone();
    v.findings = report.findings.iter().map(|f| CliFinding { finding: f.clone(), blocking: is_blocking(f) }).collect();
    v.coverage = report.coverage.clone();
    v.dropped = report.dropped;
    Ok(Some(report))
}

fn cli_markdown(v: &CliVerdict, report: Option<&Report>) -> String {
    match report {
        Some(r) => render_markdown(
            r,
            &Meta {
                base_short: short_sha(&v.base).to_string(),
                cur_short: short_sha(&v.current).to_string(),
                files: v.files,
                added: v.added,
                removed: v.removed,
                model: v.model.clone(),
            },
        ),
        None => {
            let mut body = v.summary.clone();
            if let Some(e) = &v.error {
                body = e.clone();
            }
            if let Some(raw) = &v.raw_report {
                body = format!("{body}\n\n{raw}");
            }
            format!("## Review: {}\n\n{}\n", v.verdict.to_uppercase(), body.trim())
        }
    }
}

/// `mu review`: runs the review in `cwd` and prints the verdict as JSON (or markdown) to `--out` or
/// stdout. Returns the exit code, see `exit_code`.
pub fn run_cli(o: CliOpts, cwd: &Path) -> i32 {
    let rt = match mu_base::runtime::current_thread_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mu review: {e}");
            return 2;
        }
    };
    let local = tokio::task::LocalSet::new();
    let (v, report) = local.block_on(&rt, review_once(&o, cwd));
    let text = if o.markdown {
        cli_markdown(&v, report.as_ref())
    } else {
        let mut j = serde_json::to_string_pretty(&v).unwrap_or_default();
        j.push('\n');
        j
    };
    match &o.out {
        Some(p) => {
            if let Err(e) = std::fs::write(p, &text) {
                eprintln!("mu review: cannot write {}: {e}", p.display());
                return 2;
            }
        }
        None => print!("{text}"),
    }
    match &v.error {
        Some(e) => eprintln!("mu review: error: {e}"),
        None => {
            let blocking = v.findings.iter().filter(|f| f.blocking).count();
            eprintln!("mu review: {} · {blocking} blocking finding(s)", v.verdict);
        }
    }
    exit_code(v.verdict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::sh;

    #[test]
    fn cli_nothing_to_review_and_bad_base() {
        let dir = std::env::temp_dir().join(format!("mu-review-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "init"]);
        let out = dir.join("verdict.json");
        let opts = |base: &str| CliOpts {
            base: base.into(),
            plan: None,
            focus: None,
            out: Some(out.clone()),
            markdown: false,
        };
        assert_eq!(run_cli(opts("HEAD"), &dir), 0);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["verdict"], "nothing");
        assert_eq!(run_cli(opts("no-such-ref"), &dir), 2);
        assert_eq!(exit_code("fail"), 1);
        assert_eq!(exit_code("error"), 2);
        // A missing --plan does not matter when there is nothing to review.
        let _ = std::fs::remove_file(&out); // the earlier verdict is an untracked change in the repo
        let missing_plan = CliOpts { plan: Some(dir.join("no-such-plan.md")), ..opts("HEAD") };
        assert_eq!(run_cli(missing_plan, &dir), 0);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["verdict"], "nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_outside_a_repo_reports_the_bare_error() {
        let dir = std::env::temp_dir().join(format!("mu-review-norepo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("verdict.json");
        let opts = CliOpts { base: "HEAD".into(), plan: None, focus: None, out: Some(out.clone()), markdown: false };
        assert_eq!(run_cli(opts, &dir), 2);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert!(v["error"].as_str().unwrap().starts_with("review needs a git repository: "), "{v}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
