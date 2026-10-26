use super::{ToolCtx, ToolOutput, Tools};
use crate::events::{AgentEvent, Answer, AskOption, AskQuestion};
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
    options: Vec<O>,
    #[serde(default)]
    multi: bool,
    recommended: Option<usize>,
}

#[derive(Deserialize)]
struct O {
    label: String,
    description: Option<String>,
}

pub fn description() -> &'static str {
    include_str!("../prompts/ask-tool.md")
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{"questions":{"type":"array","minItems":1,"items":{"type":"object","properties":{
        "id":{"type":"string"},
        "question":{"type":"string"},
        "header":{"type":"string"},
        "options":{"type":"array","items":{"type":"object","properties":{
            "label":{"type":"string"},"description":{"type":"string"}},"required":["label"]}},
        "multi":{"type":"boolean"},
        "recommended":{"type":"integer","description":"0-based default option index"}},
        "required":["id","question","options"]}}},"required":["questions"]})
}

fn format_answer(id: &str, a: &Answer, multi: bool) -> String {
    match a {
        Answer::Cancelled => format!("{id}: (cancelled)"),
        Answer::Custom(t) => format!("{id}: \"{t}\""),
        Answer::Options(o) if o.is_empty() => format!("{id}: []"),
        Answer::Options(o) if multi => format!("{id}: [{}]", o.join(", ")),
        Answer::Options(o) => format!("{id}: {}", o[0]),
    }
}

impl Tools {
    pub async fn ask(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let questions: Vec<AskQuestion> = a
            .questions
            .iter()
            .map(|q| AskQuestion {
                question: q.question.clone(),
                header: q.header.clone(),
                options: q
                    .options
                    .iter()
                    .map(|o| AskOption { label: o.label.clone(), description: o.description.clone() })
                    .collect(),
                multi: q.multi,
                recommended: q.recommended,
            })
            .collect();
        let (tx, rx) = oneshot::channel();
        let _ = ctx.events.send(AgentEvent::Ask { questions, reply: tx });
        let answers = tokio::select! {
            r = rx => r.ok().flatten(),
            _ = ctx.cancel.cancelled() => None,
        };
        let Some(answers) = answers else { return ToolOutput::err("Ask cancelled") };
        let lines: Vec<String> =
            a.questions.iter().zip(&answers).map(|(q, ans)| format_answer(&q.id, ans, q.multi)).collect();
        ToolOutput::ok(format!("User answers:\n{}", lines.join("\n")))
    }
}
