//! The lead reviewer's report: its schema and parsing, the blocking rule, and the markdown and
//! implementer-message renderings.
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

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
pub fn report_of(out: &mu_tools::tools::subagent::Outcome) -> Option<Report> {
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

pub fn loc(f: &Finding) -> String {
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

