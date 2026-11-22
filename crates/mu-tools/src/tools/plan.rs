use super::{ToolCtx, Tools};
use crate::context::{ModeDef, capitalize, choice_action, mode_request};
use mu_base::events::{AgentEvent, ModeAction};
use mu_base::tool::{ToolDef, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

#[derive(Deserialize)]
pub struct Args {
    slug: String,
    title: String,
}

fn schema(m: &ModeDef) -> Value {
    json!({"type":"object","properties":{
        "slug":{"type":"string","description":format!("matches {}; letters, numbers, underscores, hyphens", m.file_pattern())},
        "title":{"type":"string"}},"required":["slug","title"]})
}

impl Tools {
    /// The exit tool of a mode: it submits the mode's document for review.
    pub fn exit_def(m: &ModeDef) -> ToolDef {
        ToolDef { name: m.exit.tool.clone(), description: m.render(&m.exit.description), schema: schema(m) }
    }

    pub async fn propose(&self, a: Args, ctx: &ToolCtx, m: &ModeDef) -> ToolOutput {
        let noun = capitalize(m.noun());
        let uri = m.file_uri(&a.slug);
        if a.slug.is_empty() || !a.slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return ToolOutput::err(format!("Invalid slug `{}`: use letters, numbers, underscores, hyphens", a.slug));
        }
        let (path, content) =
            match self.resolve(&uri).ok().and_then(|r| std::fs::read_to_string(&r.path).ok().map(|c| (r.path, c))) {
                Some(v) => v,
                None => return ToolOutput::err(format!("{noun} file {uri} not found")),
            };
        let (tx, rx) = oneshot::channel();
        let request = mode_request(m, &a.title, &uri, path.clone(), content.clone(), false, None);
        let _ = ctx.events.send(AgentEvent::Review { request, reply: tx });
        let action = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return ToolOutput::err(format!("{noun} review cancelled")),
            c = rx => match c.ok().and_then(|c| choice_action(&c)) {
                Some(a) => a,
                None => return ToolOutput::err(format!("{noun} review was closed without a decision")),
            },
        };
        let content = std::fs::read_to_string(&path).unwrap_or(content);
        let title = a.title;
        let kind = action.wire_name();
        let mode = &m.name;
        let approved = |text: String, model: Option<String>, with_content: bool| {
            let mut o = ToolOutput::ok(text);
            let mut d = json!({"modeAction": kind, "mode": mode, "model": model, "uri": uri, "title": title});
            if with_content {
                d["content"] = json!(content);
            }
            o.details = Some(d);
            o
        };
        match action {
            ModeAction::Execute { model } => {
                approved(format!("{noun} approved. Executing in a fresh session."), model, true)
            }
            ModeAction::Compact { model } => approved(
                format!("{noun} approved. Compacting context, then executing it in this session."),
                model,
                true,
            ),
            ModeAction::Keep { model } => approved(format!("{noun} approved. Implement it now."), model, false),
            ModeAction::Refine(t) => ToolOutput::ok(format!("User requested changes:\n{t}")),
            ModeAction::Park => {
                approved(format!("{noun} parked. End your turn now; the user approves it later."), None, true)
            }
            ModeAction::Exit => approved(format!("{noun} accepted; {} mode is off.", m.name), None, false),
            ModeAction::Drop => ToolOutput::err(format!("{noun} review was closed without a decision")),
        }
    }
}
