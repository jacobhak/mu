//! `/review`: git facts for the lead reviewer, plus report parsing and rendering.
//! Rust gathers facts only; the lead model decides how to slice the diff.
pub mod facts;
pub mod report;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewBase {
    pub tree: Option<String>,
    pub plan_uri: Option<String>,
    pub since: i64,
}

/// `local://review-<YYYYMMDD-HHMMSS>.md`
pub fn report_uri() -> String {
    format!("local://review-{}.md", jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S"))
}

#[cfg(test)]
mod tests {
    use super::facts::*;
    use super::report::*;
    use mu_base::message::Message;
    use mu_testutil::sh;

    #[test]
    fn report_schema_accepts_a_report_and_report_of_prefers_data() {
        use mu_tools::tools::subagent::Outcome;
        let data = serde_json::json!({"verdict":"fail","summary":"s","findings":[{"title":"t","body":"b","file":"a.rs","priority":1,"confidence":0.9}]});
        assert!(mu_base::schema::check_schema(&report_schema()).is_ok());
        assert!(mu_base::schema::validate(&report_schema(), &data).is_ok());
        assert!(mu_base::schema::validate(&report_schema(), &serde_json::json!({"verdict":"maybe"})).is_err());
        let with_data = Outcome { ok: true, text: "```json\n{\"verdict\":\"pass\"}\n```".into(), data: Some(data) };
        assert_eq!(report_of(&with_data).unwrap().verdict, "fail");
        let scraped = Outcome { ok: true, text: "x\n```json\n{\"verdict\":\"pass\"}\n```".into(), data: None };
        assert!(report_of(&scraped).is_none());
    }

    #[test]
    fn report_schema_caps_line_end_line_and_dropped() {
        use mu_tools::tools::subagent::Outcome;
        use serde_json::json;
        let base = json!({"verdict":"fail","summary":"s","dropped":3,"findings":[{"title":"t","body":"b","file":"a.rs","line":12,"end_line":14,"priority":1,"confidence":0.9}]});
        assert!(mu_base::schema::validate(&report_schema(), &base).is_ok());
        let r = report_of(&Outcome { ok: true, text: String::new(), data: Some(base.clone()) }).unwrap();
        assert_eq!(r.findings[0].line, Some(12));
        assert_eq!(r.dropped, 3);
        let mutate = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut v = base.clone();
            f(&mut v);
            mu_base::schema::validate(&report_schema(), &v)
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
        use mu_tools::tools::subagent::Outcome;
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
            Message::user("I ran `ls`:\n```\na.txt\n```"),
            Message::user("I ran `cargo test` and it failed"),
            Message::user("Prior model work/tool state available. x"),
            Message::user("  "),
            Message::user("skip the test"),
        ];
        if let Message::User { timestamp, .. } = &mut msgs[0] {
            *timestamp = 1;
        }
        let n = user_notes(&msgs, 100);
        assert_eq!(n, ["I ran `cargo test` and it failed", "skip the test"]);
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
}
