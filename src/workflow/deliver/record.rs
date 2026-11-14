//! The delivery record: its fields, where it lives, and the summary written from it.

use super::steps::read_review;
use mu_agent::plans;
use mu_agent::review::report::loc;
use mu_base::git::short_sha;
use mu_common::paths::{encode_cwd, profile_home};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    pub(super) fn name(self) -> &'static str {
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

pub(super) async fn deliveries_dir(cwd: &Path) -> PathBuf {
    profile_home().join("deliveries").join(encode_cwd(&plans::main_checkout(cwd).await))
}

pub(super) fn save(dir: &Path, r: &Record) {
    let path = dir.join("delivery.json");
    let res = std::fs::create_dir_all(dir)
        .map_err(|e| e.to_string())
        .and_then(|()| serde_json::to_vec_pretty(r).map_err(|e| e.to_string()))
        .and_then(|b| mu_common::fs::write_atomic(&path, &b).map_err(|e| e.to_string()));
    if let Err(e) = res {
        eprintln!("mu deliver: cannot write {}: {e}", path.display());
    }
}

pub(super) fn load(dir: &Path) -> Result<Record, String> {
    let p = dir.join("delivery.json");
    let raw = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", p.display()))
}

pub(super) fn summary(r: &Record) -> String {
    use std::fmt::Write;
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
        short_sha(r.head.as_deref().unwrap_or("")),
        r.worktree.display(),
        short_sha(&r.base)
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
                    loc(&f.finding)
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
