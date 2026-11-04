//! `mu review`: the `/review` pipeline without a session. It gathers the same facts and request as
//! the in-session review (`review.rs`), runs `review-lead` and its workers as child processes, and
//! prints the verdict as JSON or markdown.
use crate::review::{
    Finding, Meta, PlanStep, Report, RequestInput, git, head_tree, is_blocking, numstat, render_markdown, repo_root,
    report_of, report_schema, request, snapshot_tree, split_excluded,
};
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

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
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
    let root = repo_root(cwd).await?;
    let cur = snapshot_tree(&root).await?;
    let (base, base_label) = if o.base == "HEAD" {
        (head_tree(&root).await?, "HEAD".to_string())
    } else {
        let t = git(&root, &["rev-parse", "--verify", "--quiet", &format!("{}^{{tree}}", o.base)], None)
            .await
            .map_err(|_| format!("cannot resolve --base `{}` to a tree", o.base))?;
        (t.trim().to_string(), format!("--base {}", o.base))
    };
    v.base = base.clone();
    v.current = cur.clone();
    let stats = numstat(&root, &base, &cur).await?;
    let (files, excluded) = split_excluded(stats);
    let (added, removed) =
        files.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)));
    v.files = files.len();
    v.added = added;
    v.removed = removed;
    if files.is_empty() {
        v.verdict = "nothing";
        v.summary = format!("nothing to review: no reviewable changes since {}", short(&base));
        return Ok(None);
    }
    let plan_path = match &o.plan {
        Some(p) => {
            Some(std::fs::canonicalize(cwd.join(p)).map_err(|e| format!("cannot read --plan {}: {e}", p.display()))?)
        }
        None => None,
    };
    let prompt = request(&RequestInput {
        root: &root,
        base: &base,
        cur: &cur,
        base_label: &base_label,
        plan_path,
        files: &files,
        excluded: &excluded,
        notes: &[],
        focus: o.focus.as_deref(),
    });
    let cfgs = mu_base::config::Configs::load();
    v.model = mu_base::models::role_selector("review", &cfgs).unwrap_or_else(|| "default".into());
    let exe = mu_common::paths::self_exe().map_err(|e| format!("cannot locate mu executable: {e}"))?;
    let payload =
        serde_json::json!({"task": prompt, "agent": crate::context::REVIEW_LEAD, "outputSchema": report_schema()});
    let cancel = tokio_util::sync::CancellationToken::new();
    let out = crate::tools::subagent::run_child(&exe, &root, &payload, &cancel, |name, summary| {
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
                base_short: short(&v.base),
                cur_short: short(&v.current),
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
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
