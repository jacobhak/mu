use super::{ToolCtx, ToolOutput, Tools};
use crate::events::{AgentEvent, Answer, AskOption, AskQuestion, PreviewKind};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

#[derive(Deserialize)]
pub struct Args {
    questions: Vec<Q>,
}

#[derive(Deserialize)]
struct Q {
    id: String,
    question: String,
    header: Option<String>,
    options: Option<Vec<O>>,
    #[serde(default)]
    multi: bool,
    #[serde(default)]
    confirm: bool,
    recommended: Option<usize>,
}

#[derive(Deserialize)]
struct O {
    label: String,
    description: Option<String>,
    preview: Option<String>,
    kind: Option<PreviewKind>,
}

pub fn description() -> &'static str {
    include_str!("../prompts/ask-tool.md")
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{"questions":{"type":"array","minItems":1,"items":{"type":"object","properties":{
        "id":{"type":"string"},
        "question":{"type":"string"},
        "header":{"type":"string"},
        "options":{"type":"array","minItems":2,"items":{"type":"object","properties":{
            "label":{"type":"string"},"description":{"type":"string"},"preview":{"type":"string"},
            "kind":{"type":"string","enum":["code","diff","layout","text"],"description":"what preview holds; guessed when omitted"}},"required":["label"]}},
        "multi":{"type":"boolean"},
        "confirm":{"type":"boolean","description":"yes/no question: omit options, the picker offers Yes and No"},
        "recommended":{"type":"integer","description":"0-based default option index"}},
        "required":["id","question"]}}},"required":["questions"]})
}

/// A question needs either options (at least two) or `confirm`, never both.
fn check(q: &Q) -> Result<(), String> {
    let id = &q.id;
    match &q.options {
        Some(_) if q.confirm => Err(format!("{id}: a confirm question takes no options")),
        None if !q.confirm => Err(format!("{id}: needs options (at least two), or confirm: true")),
        Some(o) if o.len() < 2 => Err(format!("{id}: needs at least two options")),
        _ if q.confirm && q.multi => Err(format!("{id}: a confirm question cannot be multi")),
        _ if q.confirm && q.recommended.is_some_and(|r| r > 1) => {
            Err(format!("{id}: recommended of a confirm question is 0 (yes) or 1 (no)"))
        }
        _ => Ok(()),
    }
}

fn question(q: &Q) -> AskQuestion {
    let option = |label: &str, description: Option<String>, preview: Option<String>, kind: Option<PreviewKind>| {
        AskOption { label: label.into(), description, preview, kind }
    };
    let options = match &q.options {
        _ if q.confirm => vec![option("Yes", None, None, None), option("No", None, None, None)],
        Some(o) => o
            .iter()
            .map(|o| {
                option(&o.label, o.description.clone(), o.preview.clone().filter(|p| !p.trim().is_empty()), o.kind)
            })
            .collect(),
        None => Vec::new(),
    };
    AskQuestion {
        question: q.question.clone(),
        header: q.header.clone(),
        options,
        multi: q.multi,
        recommended: q.recommended,
        confirm: q.confirm,
    }
}

/// The picked labels as the model reads them: `yes`/`no` for a confirm, `[a, b]` for multi.
fn pick_text(q: &Q, picks: &[String]) -> String {
    let word = |l: &str| if q.confirm { l.to_lowercase() } else { l.to_string() };
    match picks {
        [] => "[]".into(),
        _ if q.multi => format!("[{}]", picks.iter().map(|l| word(l)).collect::<Vec<_>>().join(", ")),
        [first, ..] => word(first),
    }
}

fn format_answer(q: &Q, a: &Answer) -> String {
    let id = &q.id;
    match a {
        Answer::Cancelled => format!("{id}: (cancelled)"),
        Answer::Custom(t) => format!("{id}: \"{t}\""),
        Answer::Options(o) => format!("{id}: {}", pick_text(q, o)),
        Answer::Noted { picks, note } => format!("{id}: {} — note: \"{note}\"", pick_text(q, picks)),
    }
}

impl Tools {
    pub async fn ask(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        if let Err(e) = a.questions.iter().try_for_each(check) {
            return ToolOutput::err(e);
        }
        let questions: Vec<AskQuestion> = a.questions.iter().map(question).collect();
        let (tx, rx) = oneshot::channel();
        let _ = ctx.events.send(AgentEvent::Ask { questions, reply: tx });
        let answers = tokio::select! {
            r = rx => r.ok().flatten(),
            _ = ctx.cancel.cancelled() => None,
        };
        // A dismissed dialog ends the turn like Esc on a running one; the model hears nothing of it.
        let Some(answers) = answers.filter(|a| !a.iter().any(|x| matches!(x, Answer::Cancelled))) else {
            ctx.cancel.cancel();
            return ToolOutput::err("Ask cancelled");
        };
        let lines: Vec<String> = a.questions.iter().zip(&answers).map(|(q, ans)| format_answer(q, ans)).collect();
        ToolOutput::ok(format!("User answers:\n{}", lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Args {
        serde_json::from_value(v).expect("args parse")
    }

    fn opts(labels: &[&str]) -> Value {
        labels.iter().map(|l| json!({"label": l})).collect()
    }

    #[test]
    fn old_style_call_still_parses_and_formats() {
        let a = parse(json!({"questions": [
            {"id": "pick", "question": "Which?", "options": [{"label": "A"}, {"label": "B", "description": "d"}], "recommended": 1},
            {"id": "many", "question": "Which?", "options": opts(&["X", "Y"]), "multi": true}
        ]}));
        assert!(a.questions.iter().all(|q| check(q).is_ok()));
        let qs: Vec<AskQuestion> = a.questions.iter().map(question).collect();
        assert!(qs.iter().all(|q| !q.confirm && q.options.iter().all(|o| o.preview.is_none())));
        assert_eq!(format_answer(&a.questions[0], &Answer::Options(vec!["B".into()])), "pick: B");
        assert_eq!(format_answer(&a.questions[1], &Answer::Options(vec!["X".into(), "Y".into()])), "many: [X, Y]");
        assert_eq!(format_answer(&a.questions[0], &Answer::Custom("mine".into())), "pick: \"mine\"");
        assert_eq!(format_answer(&a.questions[0], &Answer::Cancelled), "pick: (cancelled)");
    }

    #[test]
    fn schema_accepts_preview_confirm_and_optional_options() {
        let a = parse(json!({"questions": [
            {"id": "layout", "question": "Which layout?", "options": [
                {"label": "Stacked", "description": "one column", "preview": "┌──┐\n│  │\n└──┘"},
                {"label": "Split"}
            ]},
            {"id": "go", "question": "Ship it?", "confirm": true, "recommended": 1}
        ]}));
        assert!(a.questions.iter().all(|q| check(q).is_ok()));
        let qs: Vec<AskQuestion> = a.questions.iter().map(question).collect();
        assert_eq!(qs[0].options[0].preview.as_deref(), Some("┌──┐\n│  │\n└──┘"));
        assert!(qs[1].confirm);
        let labels: Vec<&str> = qs[1].options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Yes", "No"]);
        assert_eq!(schema()["properties"]["questions"]["items"]["required"], json!(["id", "question"]));
    }

    #[test]
    fn rejects_malformed_questions() {
        let bad = |v: Value| check(&parse(json!({"questions": [v]})).questions[0]);
        let base = |extra: Value| {
            let mut v = json!({"id": "q", "question": "?"});
            v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            v
        };
        assert!(bad(base(json!({}))).unwrap_err().contains("needs options"));
        assert!(bad(base(json!({"options": opts(&["only"])}))).unwrap_err().contains("at least two"));
        assert!(bad(base(json!({"options": opts(&["a", "b"]), "confirm": true}))).unwrap_err().contains("no options"));
        assert!(bad(base(json!({"confirm": true, "recommended": 2}))).unwrap_err().contains("0 (yes)"));
        assert!(bad(base(json!({"confirm": true, "multi": true}))).unwrap_err().contains("multi"));
        assert!(bad(base(json!({"confirm": true}))).is_ok());
    }

    #[test]
    fn notes_attach_to_single_multi_and_confirm_answers() {
        let a = parse(json!({"questions": [
            {"id": "s", "question": "?", "options": opts(&["Skip", "Keep"])},
            {"id": "m", "question": "?", "options": opts(&["X", "Y"]), "multi": true},
            {"id": "c", "question": "?", "confirm": true}
        ]}));
        let note = |picks: &[&str], n: &str| Answer::Noted {
            picks: picks.iter().map(|s| s.to_string()).collect(),
            note: n.into(),
        };
        assert_eq!(format_answer(&a.questions[0], &note(&["Skip"], "tests later")), "s: Skip — note: \"tests later\"");
        assert_eq!(format_answer(&a.questions[1], &note(&["X", "Y"], "both")), "m: [X, Y] — note: \"both\"");
        assert_eq!(format_answer(&a.questions[2], &note(&["No"], "because")), "c: no — note: \"because\"");
        assert_eq!(format_answer(&a.questions[2], &Answer::Options(vec!["Yes".into()])), "c: yes");
        assert_eq!(format_answer(&a.questions[2], &Answer::Custom("maybe".into())), "c: \"maybe\"");
    }
}
