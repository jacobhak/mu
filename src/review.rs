//! `/review`: git facts for the lead reviewer, plus report parsing and rendering.
//! Rust gathers facts only; the lead model decides how to slice the diff.
use crate::message::{Message, text_of};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_FILES_LISTED: usize = 400;

const REQUEST: &str = include_str!("prompts/review-request.md");

/// Runs git in `dir`; stdout on success, git's first stderr line otherwise.
pub(crate) async fn git(dir: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let out = match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("git {}: {e}", args.first().copied().unwrap_or(""))),
        Err(_) => return Err(format!("git {} timed out", args.first().copied().unwrap_or(""))),
    };
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().next().unwrap_or("git failed").trim().to_string())
    }
}

pub async fn repo_root(cwd: &Path) -> Result<PathBuf, String> {
    match git(cwd, &["rev-parse", "--show-toplevel"], None).await {
        Ok(s) => Ok(PathBuf::from(s.trim())),
        Err(e) => Err(format!("review needs a git repository: {e}")),
    }
}

/// A tree sha of the whole working tree, untracked non-ignored files included. Uses a copy of the
/// index, so neither the real index nor the working tree is touched.
pub async fn snapshot_tree(cwd: &Path) -> Result<String, String> {
    let real = git(cwd, &["rev-parse", "--git-path", "index"], None).await?;
    let real = cwd.join(real.trim());
    let tmp = std::env::temp_dir().join(format!("mu-review-index-{}", uuid::Uuid::new_v4()));
    let res = async {
        if real.exists() {
            std::fs::copy(&real, &tmp).map_err(|e| format!("cannot copy index: {e}"))?;
            // Git's racy-entry check compares index entries with the index file's mtime; the copy must keep
            // the real index's mtime, or a same-tick rewrite of a tracked file looks unchanged.
            let mtime =
                std::fs::metadata(&real).and_then(|m| m.modified()).map_err(|e| format!("cannot stat index: {e}"))?;
            std::fs::File::options()
                .write(true)
                .open(&tmp)
                .and_then(|f| f.set_modified(mtime))
                .map_err(|e| format!("cannot stamp index copy: {e}"))?;
        }
        git(cwd, &["add", "-A"], Some(&tmp)).await?;
        git(cwd, &["write-tree"], Some(&tmp)).await
    }
    .await;
    let _ = std::fs::remove_file(&tmp);
    res.map(|s| s.trim().to_string())
}

pub async fn head_tree(cwd: &Path) -> Result<String, String> {
    match git(cwd, &["rev-parse", "HEAD^{tree}"], None).await {
        Ok(s) => Ok(s.trim().to_string()),
        Err(_) => git(cwd, &["hash-object", "-t", "tree", "/dev/null"], None).await.map(|s| s.trim().to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    /// `None` for a binary file.
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

impl FileStat {
    fn changed(&self) -> u32 {
        self.added.unwrap_or(0) + self.removed.unwrap_or(0)
    }
}

pub async fn numstat(root: &Path, base: &str, cur: &str) -> Result<Vec<FileStat>, String> {
    git(root, &["diff", "--numstat", "-z", "--no-renames", base, cur], None).await.map(|s| parse_numstat(&s))
}

pub fn parse_numstat(raw: &str) -> Vec<FileStat> {
    raw.split('\0')
        .filter_map(|rec| {
            let mut it = rec.trim_start_matches('\n').splitn(3, '\t');
            let (a, r, path) = (it.next()?, it.next()?, it.next()?);
            Some(FileStat { path: path.into(), added: a.parse().ok(), removed: r.parse().ok() })
        })
        .collect()
}

pub fn excluded_reason(path: &str, binary: bool) -> Option<&'static str> {
    if binary {
        return Some("binary");
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.ends_with(".lock")
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
        || name.ends_with("-lock.yml")
    {
        Some("lock file")
    } else if name.ends_with(".min.js") || name.ends_with(".min.css") {
        Some("minified")
    } else if path.contains(".generated.") {
        Some("generated")
    } else if name.ends_with(".snap") {
        Some("snapshot")
    } else if name.ends_with(".map") {
        Some("source map")
    } else if ["dist/", "build/", "out/"].iter().any(|p| path.starts_with(p)) {
        Some("build output")
    } else if path.contains("node_modules/") || path.contains("vendor/") {
        Some("vendor")
    } else {
        None
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewBase {
    pub tree: Option<String>,
    pub plan_uri: Option<String>,
    pub since: i64,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max).collect::<String>()) }
}

/// What the user told the implementer since the plan was approved.
pub fn user_notes(messages: &[Message], since: i64) -> Vec<String> {
    let mut out: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content, timestamp, synthetic: false } if *timestamp >= since => {
                let t = text_of(content);
                let t = t.trim();
                let skip = t.is_empty()
                    || ["<system-notice>", "<system-reminder>", "I ran `", "Prior model work/tool state available."]
                        .iter()
                        .any(|p| t.starts_with(p));
                (!skip).then(|| truncate(t, 1500))
            }
            _ => None,
        })
        .collect();
    let drop = out.len().saturating_sub(15);
    out.drain(..drop);
    out
}

pub struct RequestInput<'a> {
    pub root: &'a Path,
    pub base: &'a str,
    pub cur: &'a str,
    pub base_label: &'a str,
    pub plan_path: Option<PathBuf>,
    pub files: &'a [FileStat],
    pub excluded: &'a [(FileStat, &'a str)],
    pub notes: &'a [String],
    pub focus: Option<&'a str>,
}

fn stat_cell(f: &FileStat) -> String {
    match (f.added, f.removed) {
        (Some(a), Some(r)) => format!("+{a}/-{r}"),
        _ => "binary".into(),
    }
}

pub fn request(r: &RequestInput) -> String {
    let mut sorted: Vec<&FileStat> = r.files.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse(f.changed()));
    let mut files = String::from("| path | +a/-r |\n|---|---|\n");
    for f in sorted.iter().take(MAX_FILES_LISTED) {
        let _ = writeln!(files, "| {} | {} |", f.path, stat_cell(f));
    }
    if sorted.len() > MAX_FILES_LISTED {
        let _ = writeln!(
            files,
            "… {} more files: run `git diff --stat {} {}`",
            sorted.len() - MAX_FILES_LISTED,
            r.base,
            r.cur
        );
    }
    let excluded = if r.excluded.is_empty() {
        "none".to_string()
    } else {
        r.excluded.iter().map(|(f, why)| format!("- {} ({why})", f.path)).collect::<Vec<_>>().join("\n")
    };
    let (a, d) = r.files.iter().fold((0, 0), |(a, d), f| (a + f.added.unwrap_or(0), d + f.removed.unwrap_or(0)));
    let totals = format!("{} reviewable files, +{a}/-{d} lines", r.files.len());
    let plan = match &r.plan_path {
        Some(p) => format!("The approved plan is at `{}`. Read it first.", p.display()),
        None => "No plan: review for correctness and quality only.".into(),
    };
    let notes = if r.notes.is_empty() {
        "none".to_string()
    } else {
        r.notes.iter().enumerate().map(|(i, n)| format!("{}. {n}", i + 1)).collect::<Vec<_>>().join("\n")
    };
    REQUEST
        .replace("{{root}}", &r.root.display().to_string())
        .replace("{{base_label}}", r.base_label)
        .replace("{{base}}", r.base)
        .replace("{{cur}}", r.cur)
        .replace("{{totals}}", &totals)
        .replace("{{plan}}", &plan)
        .replace("{{files}}", files.trim_end())
        .replace("{{excluded}}", &excluded)
        .replace("{{notes}}", &notes)
        .replace("{{focus}}", r.focus.unwrap_or("none"))
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Report {
    pub verdict: String,
    pub summary: String,
    pub plan: Vec<PlanStep>,
    pub findings: Vec<Finding>,
    pub coverage: String,
    pub dropped: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
    pub evidence: String,
    pub sanctioned: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Finding {
    pub title: String,
    pub body: String,
    pub file: String,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub priority: u8,
    pub confidence: f32,
    pub kind: String,
}

impl Default for Finding {
    fn default() -> Self {
        Self {
            title: String::new(),
            body: String::new(),
            file: String::new(),
            line: None,
            end_line: None,
            priority: 2,
            confidence: 1.0,
            kind: String::new(),
        }
    }
}

fn clamp(mut r: Report) -> Option<Report> {
    for f in &mut r.findings {
        f.priority = f.priority.min(3);
        f.confidence = if f.confidence.is_nan() { 1.0 } else { f.confidence.clamp(0.0, 1.0) };
    }
    Some(r)
}

/// The lead's report: its schema-validated `data`. A run without `data` has no report.
pub fn report_of(out: &crate::tools::subagent::Outcome) -> Option<Report> {
    clamp(serde_json::from_value(out.data.clone()?).ok()?)
}

/// The result schema of the lead reviewer (`outputSchema` of its task).
pub fn report_schema() -> serde_json::Value {
    serde_json::json!({"type":"object","required":["verdict","summary","findings"],"properties":{
        "verdict":{"enum":["pass","fail"]},
        "summary":{"type":"string"},
        "plan":{"type":"array","items":{"type":"object","required":["step","status"],"properties":{
            "step":{"type":"string"},
            "status":{"enum":["done","partial","missing","deviated"]},
            "evidence":{"type":"string"},
            "sanctioned":{"type":"boolean"}}}},
        "findings":{"type":"array","items":{"type":"object","required":["title","body","file","priority","confidence"],"properties":{
            "title":{"type":"string"},
            "body":{"type":"string"},
            "file":{"type":"string"},
            "line":{"type":"integer","minimum":1,"maximum":1000000},
            "end_line":{"type":"integer","minimum":1,"maximum":1000000},
            "priority":{"type":"integer","minimum":0,"maximum":3},
            "confidence":{"type":"number","minimum":0,"maximum":1},
            "kind":{"enum":["bug","plan","quality"]}}}},
        "coverage":{"type":"string"},
        "dropped":{"type":"integer","minimum":0,"maximum":10000}}})
}

pub fn is_blocking(f: &Finding) -> bool {
    f.priority <= 2 && f.confidence >= 0.6
}

pub struct Meta {
    pub base_short: String,
    pub cur_short: String,
    pub files: usize,
    pub added: u32,
    pub removed: u32,
    pub model: String,
}

pub(crate) fn loc(f: &Finding) -> String {
    match (f.line, f.end_line) {
        (Some(l), Some(e)) if e != l => format!("{}:{l}-{e}", f.file),
        (Some(l), _) => format!("{}:{l}", f.file),
        _ => f.file.clone(),
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

pub fn render_markdown(r: &Report, meta: &Meta) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "## Review: {}\n", if r.verdict == "pass" { "PASS" } else { "NEEDS FIXES" });
    if !r.summary.is_empty() {
        let _ = writeln!(o, "{}\n", r.summary.trim());
    }
    let _ = write!(
        o,
        "`{}`→`{}` · {} files +{}/−{} · reviewer {}",
        meta.base_short, meta.cur_short, meta.files, meta.added, meta.removed, meta.model
    );
    if r.dropped > 0 {
        let _ = write!(o, " · {} candidate findings dropped in verification", r.dropped);
    }
    o.push_str("\n\n");
    if !r.plan.is_empty() {
        o.push_str("### Plan adherence\n\n| Step | Status | Evidence |\n|---|---|---|\n");
        for s in &r.plan {
            let status = if s.status == "deviated" && s.sanctioned { "deviated (user-approved)" } else { &s.status };
            let _ = writeln!(o, "| {} | {} | {} |", cell(&s.step), cell(status), cell(&s.evidence));
        }
        o.push('\n');
    }
    let (blocking, minor): (Vec<&Finding>, Vec<&Finding>) = r.findings.iter().partition(|f| is_blocking(f));
    if !blocking.is_empty() {
        o.push_str("### Findings\n\n");
        for (i, f) in blocking.iter().enumerate() {
            let _ = writeln!(o, "**{}. [P{}] {}** — `{}`\n\n{}\n", i + 1, f.priority, f.title, loc(f), f.body.trim());
        }
    }
    if !minor.is_empty() {
        o.push_str("### Minor (not sent by default)\n\n");
        for f in minor {
            let _ = writeln!(o, "- [P{}] {} — `{}`", f.priority, f.title, loc(f));
        }
        o.push('\n');
    }
    if !r.coverage.trim().is_empty() {
        let _ = write!(o, "### Coverage\n\n{}\n", r.coverage.trim());
    }
    o
}

fn fix_intro(plan_uri: Option<&str>) -> String {
    let plan = plan_uri.map(|u| format!(" against the plan at `{u}`")).unwrap_or_default();
    format!(
        "A reviewer checked your implementation{plan}. Fix the findings below, verify each fix, then report what changed. If a finding is wrong, say why instead of changing code."
    )
}

fn fix_tail(o: &mut String, report_uri: &str, note: Option<&str>) {
    if let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) {
        let _ = write!(o, "\nUser note: {n}\n");
    }
    let _ = write!(o, "\nFull report: {report_uri}");
}

/// The user message that hands findings to the implementer.
pub fn fix_message(findings: &[&Finding], plan_uri: Option<&str>, report_uri: &str, note: Option<&str>) -> String {
    let mut o = fix_intro(plan_uri);
    o.push_str("\n\n");
    for (i, f) in findings.iter().enumerate() {
        let _ = writeln!(o, "{}. [P{}] {} ({})\n{}\n", i + 1, f.priority, f.title, loc(f), f.body.trim());
    }
    fix_tail(&mut o, report_uri, note);
    o
}

/// Same hand-off for a report that could not be parsed: the raw text stands in for the findings.
pub fn raw_message(text: &str, plan_uri: Option<&str>, report_uri: &str) -> String {
    let mut o = fix_intro(plan_uri);
    let _ = write!(o, "\n\n{}\n", text.trim());
    fix_tail(&mut o, report_uri, None);
    o
}

/// `local://review-<YYYYMMDD-HHMMSS>.md`
pub fn report_uri() -> String {
    format!("local://review-{}.md", jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S"))
}

/// Splits changed files into the reviewable ones and `(file, reason)` exclusions.
pub fn split_excluded(stats: Vec<FileStat>) -> (Vec<FileStat>, Vec<(FileStat, &'static str)>) {
    let (mut files, mut excluded) = (Vec::new(), Vec::new());
    for f in stats {
        match excluded_reason(&f.path, f.added.is_none()) {
            Some(why) => excluded.push((f, why)),
            None => files.push(f),
        }
    }
    (files, excluded)
}

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
    let cfgs = crate::config::Configs::load();
    v.model = crate::models::role_selector("review", &cfgs).unwrap_or_else(|| "default".into());
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
    fn report_schema_accepts_a_report_and_report_of_prefers_data() {
        use crate::tools::subagent::Outcome;
        let data = serde_json::json!({"verdict":"fail","summary":"s","findings":[{"title":"t","body":"b","file":"a.rs","priority":1,"confidence":0.9}]});
        assert!(crate::schema::check_schema(&report_schema()).is_ok());
        assert!(crate::schema::validate(&report_schema(), &data).is_ok());
        assert!(crate::schema::validate(&report_schema(), &serde_json::json!({"verdict":"maybe"})).is_err());
        let with_data = Outcome { ok: true, text: "```json\n{\"verdict\":\"pass\"}\n```".into(), data: Some(data) };
        assert_eq!(report_of(&with_data).unwrap().verdict, "fail");
        let scraped = Outcome { ok: true, text: "x\n```json\n{\"verdict\":\"pass\"}\n```".into(), data: None };
        assert!(report_of(&scraped).is_none());
    }

    #[test]
    fn report_schema_caps_line_end_line_and_dropped() {
        use crate::tools::subagent::Outcome;
        use serde_json::json;
        let base = json!({"verdict":"fail","summary":"s","dropped":3,"findings":[{"title":"t","body":"b","file":"a.rs","line":12,"end_line":14,"priority":1,"confidence":0.9}]});
        assert!(crate::schema::validate(&report_schema(), &base).is_ok());
        let r = report_of(&Outcome { ok: true, text: String::new(), data: Some(base.clone()) }).unwrap();
        assert_eq!(r.findings[0].line, Some(12));
        assert_eq!(r.dropped, 3);
        let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut v = base.clone();
            f(&mut v);
            crate::schema::validate(&report_schema(), &v)
        };
        assert!(mutate(&|v| v["findings"][0]["line"] = json!(-5)).is_err());
        assert!(mutate(&|v| v["findings"][0]["line"] = json!(0)).is_err());
        assert!(mutate(&|v| v["findings"][0]["line"] = json!(1000001)).is_err());
        assert!(mutate(&|v| v["findings"][0]["end_line"] = json!(4294967296u64)).is_err());
        assert!(mutate(&|v| v["findings"][0]["end_line"] = json!(-1)).is_err());
        assert!(mutate(&|v| v["dropped"] = json!(10001)).is_err());
        assert!(mutate(&|v| v["dropped"] = json!(-1)).is_err());
        assert!(mutate(&|v| v["findings"][0]["line"] = json!(1000000)).is_ok());
        assert!(mutate(&|v| v["findings"][0]["end_line"] = json!(1000000)).is_ok());
        assert!(mutate(&|v| v["dropped"] = json!(10000)).is_ok());
    }

    fn finding(p: u8, c: f32) -> Finding {
        Finding {
            title: "t".into(),
            file: "a.rs".into(),
            line: Some(3),
            priority: p,
            confidence: c,
            ..Default::default()
        }
    }

    #[test]
    fn numstat_marks_binary() {
        let s = parse_numstat("3\t1\tsrc/a.rs\0-\t-\timg.png\0");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], FileStat { path: "src/a.rs".into(), added: Some(3), removed: Some(1) });
        assert_eq!(s[1].added, None);
    }

    #[test]
    fn exclusions() {
        assert_eq!(excluded_reason("Cargo.lock", false), Some("lock file"));
        assert_eq!(excluded_reason("web/package-lock.json", false), Some("lock file"));
        assert_eq!(excluded_reason("a.png", true), Some("binary"));
        assert_eq!(excluded_reason("dist/x.js", false), Some("build output"));
        assert_eq!(excluded_reason("src/a.rs", false), None);
    }

    #[test]
    fn report_of_clamps() {
        use crate::tools::subagent::Outcome;
        let d = serde_json::json!({"verdict":"fail","findings":[{"title":"a","priority":7,"confidence":3.0},{}]});
        let r = report_of(&Outcome { ok: true, text: String::new(), data: Some(d) }).unwrap();
        assert_eq!((r.findings[0].priority, r.findings[0].confidence), (3, 1.0));
        assert_eq!((r.findings[1].priority, r.findings[1].confidence), (2, 1.0));
        assert!(report_of(&Outcome { ok: true, text: "```json\n{}\n```".into(), data: None }).is_none());
    }

    #[test]
    fn blocking_set() {
        assert!(is_blocking(&finding(2, 0.6)));
        assert!(!is_blocking(&finding(3, 1.0)));
        assert!(!is_blocking(&finding(1, 0.4)));
    }

    #[test]
    fn notes_filter_synthetic_and_system_text() {
        let mut msgs = vec![
            Message::user("old"),
            Message::synthetic_user("plan"),
            Message::user("<system-notice>x"),
            Message::user("<system-reminder>x"),
            Message::user("I ran `ls`:"),
            Message::user("Prior model work/tool state available. x"),
            Message::user("  "),
            Message::user("skip the test"),
        ];
        if let Message::User { timestamp, .. } = &mut msgs[0] {
            *timestamp = 1;
        }
        let n = user_notes(&msgs, 100);
        assert_eq!(n, ["skip the test"]);
    }

    fn meta() -> Meta {
        Meta {
            base_short: "aaaaaaa".into(),
            cur_short: "bbbbbbb".into(),
            files: 2,
            added: 3,
            removed: 1,
            model: "m".into(),
        }
    }

    #[test]
    fn markdown_sections() {
        let r = Report {
            verdict: "pass".into(),
            plan: vec![PlanStep {
                step: "tests".into(),
                status: "deviated".into(),
                sanctioned: true,
                ..Default::default()
            }],
            findings: vec![finding(3, 1.0)],
            ..Default::default()
        };
        let md = render_markdown(&r, &meta());
        assert!(md.starts_with("## Review: PASS"));
        assert!(md.contains("deviated (user-approved)"));
        assert!(md.contains("### Minor"));
        assert!(!md.contains("### Findings"));
    }

    #[test]
    fn fix_message_shape() {
        let f = finding(1, 0.9);
        let m = fix_message(&[&f], Some("local://p-plan.md"), "local://r.md", Some("careful"));
        assert!(m.starts_with("A reviewer checked your implementation against the plan at `local://p-plan.md`."));
        assert!(m.contains("1. [P1] t (a.rs:3)") && m.contains("User note: careful") && m.ends_with("local://r.md"));
    }

    #[test]
    fn snapshot_sees_changes_and_leaves_index_alone() {
        let dir = std::env::temp_dir().join(format!("mu-review-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "init"]);
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("new.txt"), "n\n").unwrap();
        let before = sh(&dir, &["status", "--porcelain"]);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (head, snap, stats) = rt.block_on(async {
            let head = head_tree(&dir).await.unwrap();
            let snap = snapshot_tree(&dir).await.unwrap();
            let stats = numstat(&dir, &head, &snap).await.unwrap();
            (head, snap, stats)
        });
        assert_ne!(head, snap);
        let mut paths: Vec<&str> = stats.iter().map(|s| s.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["a.txt", "new.txt"]);
        assert_eq!(sh(&dir, &["status", "--porcelain"]), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
