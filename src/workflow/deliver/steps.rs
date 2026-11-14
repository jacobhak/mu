//! What a delivery runs: the steps (each a `mu` child process), the review report they leave, and
//! the prompts that go back to the implementer.

use mu_agent::plans;
use mu_agent::review::report::{Finding, fix_message, raw_message};
use mu_session::session::inbox;
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;

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
            // Approving the plan for detached delivery is the authorization; deny rules still apply.
            .args(["--permissions", "yolo"])
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
        match inbox::send(session, Some("mu deliver"), text, dir) {
            Ok(l) => l,
            Err(e) => format!("not sent: {e}"),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct ReviewOut {
    pub verdict: String,
    pub error: Option<String>,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Deserialize)]
pub(super) struct ReviewFinding {
    #[serde(flatten)]
    pub finding: Finding,
    #[serde(default)]
    pub blocking: bool,
}

pub(super) fn read_review(path: &Path) -> ReviewOut {
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

pub(super) const COMMIT_PROMPT: &str = "Commit your work now. Stage only the files that belong to this change (no build output, caches or scratch files), commit with a message that describes the whole change, and reply with the commit and any files you left uncommitted and why.";

pub(super) fn fix_prompt(r: &ReviewOut, plan: Option<&Path>, report: &Path) -> String {
    let plan_uri = plan.map(|p| p.display().to_string());
    let report_uri = report.display().to_string();
    let blocking: Vec<&Finding> = r.findings.iter().filter(|f| f.blocking).map(|f| &f.finding).collect();
    let picked = if blocking.is_empty() { r.findings.iter().map(|f| &f.finding).collect() } else { blocking };
    if picked.is_empty() {
        raw_message(&format!("Reviewer summary: {}", r.summary), plan_uri.as_deref(), &report_uri)
    } else {
        fix_message(&picked, plan_uri.as_deref(), &report_uri, None)
    }
}
