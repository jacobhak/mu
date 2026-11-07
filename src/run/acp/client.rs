//! What the agent asks of the client: approval of a plan (`session/request_permission`) and the
//! model's `ask` questions (`elicitation/create`).

use mu_base::events::{Answer, AskQuestion, PlanDecision};
use mu_base::permissions::{ApprovalDecision, ApprovalRequest, ApprovalScope};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::oneshot;

/// A client's answer: its `result`, or its `error` object.
pub(super) type Reply = Result<Value, Value>;

/// Requests to the client, matched with their responses by id.
#[derive(Clone)]
pub(super) struct Rpc {
    next: Rc<Cell<u64>>,
    pending: Rc<RefCell<HashMap<u64, oneshot::Sender<Reply>>>>,
    out: Rc<dyn Fn(&Value)>,
}

impl Rpc {
    pub fn new(out: Rc<dyn Fn(&Value)>) -> Self {
        Self { next: Rc::new(Cell::new(1)), pending: Rc::default(), out }
    }

    pub fn request(&self, method: &str, params: Value) -> oneshot::Receiver<Reply> {
        let id = self.next.get();
        self.next.set(id + 1);
        let (tx, rx) = oneshot::channel();
        self.pending.borrow_mut().insert(id, tx);
        (self.out)(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx
    }

    /// Delivers a response. `false` when no request of ours has that id.
    pub fn resolve(&self, id: &Value, reply: Reply) -> bool {
        let Some(tx) = id.as_u64().and_then(|n| self.pending.borrow_mut().remove(&n)) else { return false };
        let _ = tx.send(reply);
        true
    }
}

/// The plan approval options: id, name, kind. Approvals come first, so a client that auto-approves
/// picks to implement in this conversation.
const PLAN_OPTIONS: [(&str, &str, &str); 5] = [
    ("keep", "Approve and implement now, keeping this conversation", "allow_once"),
    ("execute", "Approve and implement in a fresh session", "allow_once"),
    ("compact", "Approve, compact the conversation, then implement", "allow_once"),
    ("park", "Save the plan for later and do not implement it", "reject_once"),
    ("refine", "Keep planning", "reject_once"),
];

pub(super) fn plan_request(sid: &str, call_id: &str, title: &str, content: &str) -> Value {
    let options: Vec<Value> =
        PLAN_OPTIONS.iter().map(|(id, name, kind)| json!({"optionId": id, "name": name, "kind": kind})).collect();
    json!({
        "sessionId": sid,
        "toolCall": {
            "toolCallId": call_id,
            "title": format!("Plan: {title}"),
            "kind": "switch_mode",
            "status": "pending",
            "content": [{"type": "content", "content": {"type": "text", "text": content}}],
        },
        "options": options,
    })
}

/// What a `session/request_permission` result decides. Anything but a known approval is no approval.
pub(super) fn plan_decision(result: &Value) -> PlanDecision {
    let o = &result["outcome"];
    if o["outcome"] != "selected" {
        return PlanDecision::Drop;
    }
    match o["optionId"].as_str() {
        Some("keep") => PlanDecision::Keep { model: None },
        Some("execute") => PlanDecision::Execute { model: None },
        Some("compact") => PlanDecision::Compact { model: None },
        Some("park") => PlanDecision::Park,
        _ => PlanDecision::Refine("The plan is not approved yet. Stop and wait for the user's instructions.".into()),
    }
}

/// A tool call's approval options: answering once, then the scopes the prompt offers a grant for.
const APPROVAL_OPTIONS: [(&str, &str, &str); 5] = [
    ("once", "Yes", "allow_once"),
    ("session", "Yes, this session", "allow_always"),
    ("project", "Always in this project", "allow_always"),
    ("global", "Always everywhere", "allow_always"),
    ("deny", "No", "reject_once"),
];

pub(super) fn approval_request(sid: &str, req: &ApprovalRequest) -> Value {
    let options: Vec<Value> = APPROVAL_OPTIONS
        .iter()
        .filter(|(id, ..)| req.grant.is_some() || !matches!(*id, "session" | "project" | "global"))
        .map(|(id, name, kind)| json!({"optionId": id, "name": name, "kind": kind}))
        .collect();
    let text = format!("{}\n\n{}", req.reason, req.body);
    json!({
        "sessionId": sid,
        "toolCall": {
            "toolCallId": req.call_id,
            "title": req.title,
            "status": "pending",
            "content": [{"type": "content", "content": {"type": "text", "text": text}}],
        },
        "options": options,
    })
}

/// What a tool approval's answer decides. Anything but a known approval is a denial.
pub(super) fn approval_decision(result: &Value) -> ApprovalDecision {
    let o = &result["outcome"];
    if o["outcome"] != "selected" {
        return ApprovalDecision::Deny { note: None };
    }
    let scope = match o["optionId"].as_str() {
        Some("once") => ApprovalScope::Once,
        Some("session") => ApprovalScope::Session,
        Some("project") => ApprovalScope::Project,
        Some("global") => ApprovalScope::Global,
        _ => return ApprovalDecision::Deny { note: None },
    };
    ApprovalDecision::Allow { scope, note: None }
}

pub(super) async fn approve_tool(
    rpc: Rpc,
    sid: String,
    req: ApprovalRequest,
    reply: oneshot::Sender<ApprovalDecision>,
) {
    let rx = rpc.request("session/request_permission", approval_request(&sid, &req));
    let decision = match rx.await {
        Ok(Ok(v)) => approval_decision(&v),
        _ => ApprovalDecision::Deny { note: None },
    };
    let _ = reply.send(decision);
}

pub(super) async fn review_plan(
    rpc: Rpc,
    sid: String,
    call_id: String,
    title: String,
    content: String,
    reply: oneshot::Sender<PlanDecision>,
) {
    let rx = rpc.request("session/request_permission", plan_request(&sid, &call_id, &title, &content));
    let decision = match rx.await {
        Ok(Ok(v)) => plan_decision(&v),
        _ => PlanDecision::Drop,
    };
    let _ = reply.send(decision);
}

fn choices(q: &AskQuestion) -> Vec<Value> {
    q.options
        .iter()
        .map(|o| {
            let title = match o.description.as_deref().filter(|d| !d.is_empty()) {
                Some(d) => format!("{} — {d}", o.label),
                None => o.label.clone(),
            };
            json!({"const": o.label, "title": title})
        })
        .collect()
}

/// The form for the questions: a pick per question, plus an optional typed answer instead.
pub(super) fn elicitation_params(sid: &str, questions: &[AskQuestion]) -> Value {
    let mut props = serde_json::Map::new();
    for (i, q) in questions.iter().enumerate() {
        let title = q.header.clone().unwrap_or_else(|| format!("Question {}", i + 1));
        let description = match q.context.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => format!("{}\n{c}", q.question),
            None => q.question.clone(),
        };
        let mut pick = if q.multi {
            json!({"type": "array", "title": title, "description": description, "uniqueItems": true, "items": {"anyOf": choices(q)}})
        } else {
            json!({"type": "string", "title": title, "description": description, "oneOf": choices(q)})
        };
        if let (false, Some(r)) = (q.multi, q.recommended.and_then(|r| q.options.get(r))) {
            pick["default"] = json!(r.label);
        }
        props.insert(format!("q{i}"), pick);
        props.insert(
            format!("q{i}_other"),
            json!({"type": "string", "title": "Other", "description": "Your own answer, instead of the choices above"}),
        );
    }
    let message = match questions {
        [q] => q.question.clone(),
        _ => "mu has some questions".to_string(),
    };
    json!({
        "sessionId": sid,
        "mode": "form",
        "message": message,
        "requestedSchema": {"type": "object", "properties": Value::Object(props)},
    })
}

/// The answers in an accepted form, one per question; a question left empty is `Skipped` when it
/// can be, else `Cancelled`.
pub(super) fn answers_from(content: &Value, questions: &[AskQuestion]) -> Vec<Answer> {
    (0..questions.len())
        .map(|i| {
            if let Some(t) = content[format!("q{i}_other")].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                return Answer::Custom(t.into());
            }
            match &content[format!("q{i}")] {
                Value::String(s) if !s.is_empty() => Answer::Options(vec![s.clone()]),
                Value::Array(a) if !a.is_empty() => {
                    Answer::Options(a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                }
                _ if questions[i].skippable => Answer::Skipped,
                _ => Answer::Cancelled,
            }
        })
        .collect()
}

pub(super) async fn ask_user(
    rpc: Rpc,
    sid: String,
    questions: Vec<AskQuestion>,
    reply: oneshot::Sender<Option<Vec<Answer>>>,
) {
    let rx = rpc.request("elicitation/create", elicitation_params(&sid, &questions));
    let answers = match rx.await {
        Ok(Ok(v)) if v["action"] == "accept" => Some(answers_from(&v["content"], &questions)),
        _ => None,
    };
    let _ = reply.send(answers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::events::AskOption;

    fn q(question: &str, labels: &[&str], multi: bool) -> AskQuestion {
        AskQuestion {
            question: question.into(),
            context: None,
            header: Some("Pick".into()),
            options: labels
                .iter()
                .map(|l| AskOption { label: (*l).into(), description: None, preview: None, kind: None })
                .collect(),
            multi,
            recommended: Some(1),
            confirm: false,
            skippable: false,
        }
    }

    #[test]
    fn requests_go_out_with_increasing_ids_and_responses_find_their_request() {
        let sent = Rc::new(RefCell::new(Vec::<Value>::new()));
        let log = sent.clone();
        let rpc = Rpc::new(Rc::new(move |v: &Value| log.borrow_mut().push(v.clone())));
        let mut a = rpc.request("m/a", json!({"x": 1}));
        let mut b = rpc.request("m/b", json!({}));
        let s = sent.borrow();
        assert_eq!(
            (s[0]["id"].as_u64(), s[0]["method"].as_str(), s[1]["id"].as_u64()),
            (Some(1), Some("m/a"), Some(2))
        );
        assert!(rpc.resolve(&json!(2), Ok(json!("two"))));
        assert!(!rpc.resolve(&json!(2), Ok(json!("again"))), "answered once");
        assert!(!rpc.resolve(&json!("1"), Ok(json!(1))), "a string id is not ours");
        assert_eq!(b.try_recv().unwrap().unwrap(), json!("two"));
        assert!(a.try_recv().is_err(), "still waiting");
        assert!(rpc.resolve(&json!(1), Err(json!({"code": -1}))));
        assert!(a.try_recv().unwrap().is_err());
    }

    #[test]
    fn plan_options_put_approvals_first_and_unknown_outcomes_are_no_approval() {
        let p = plan_request("s", "call", "Add x", "the plan");
        let kinds: Vec<&str> = p["options"].as_array().unwrap().iter().map(|o| o["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["allow_once", "allow_once", "allow_once", "reject_once", "reject_once"]);
        assert_eq!(
            (p["toolCall"]["kind"].as_str(), p["toolCall"]["title"].as_str()),
            (Some("switch_mode"), Some("Plan: Add x"))
        );
        let pick = |id: &str| plan_decision(&json!({"outcome": {"outcome": "selected", "optionId": id}}));
        assert!(matches!(pick("keep"), PlanDecision::Keep { model: None }));
        assert!(matches!(pick("execute"), PlanDecision::Execute { .. }));
        assert!(matches!(pick("compact"), PlanDecision::Compact { .. }));
        assert!(matches!(pick("park"), PlanDecision::Park));
        assert!(matches!(pick("refine"), PlanDecision::Refine(_)));
        assert!(matches!(pick("something else"), PlanDecision::Refine(_)));
        assert!(matches!(plan_decision(&json!({"outcome": {"outcome": "cancelled"}})), PlanDecision::Drop));
        assert!(matches!(plan_decision(&json!({})), PlanDecision::Drop));
    }

    #[test]
    fn tool_approvals_offer_the_grant_scopes_and_map_back() {
        let req = |grant: bool| ApprovalRequest {
            tool: "bash".into(),
            call_id: "c".into(),
            title: "bash: touch x".into(),
            reason: "ask mode: bash needs approval".into(),
            body: "touch x".into(),
            from: None,
            grant: grant.then(|| mu_base::permissions::Grant { tool: "bash".into(), patterns: vec!["touch *".into()] }),
        };
        let ids = |v: &Value| -> Vec<String> {
            v["options"].as_array().unwrap().iter().map(|o| o["optionId"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(ids(&approval_request("s", &req(false))), ["once", "deny"]);
        assert_eq!(ids(&approval_request("s", &req(true))), ["once", "session", "project", "global", "deny"]);
        let pick = |id: &str| approval_decision(&json!({"outcome": {"outcome": "selected", "optionId": id}}));
        assert!(matches!(pick("once"), ApprovalDecision::Allow { scope: ApprovalScope::Once, .. }));
        assert!(matches!(pick("session"), ApprovalDecision::Allow { scope: ApprovalScope::Session, .. }));
        assert!(matches!(pick("project"), ApprovalDecision::Allow { scope: ApprovalScope::Project, .. }));
        assert!(matches!(pick("global"), ApprovalDecision::Allow { scope: ApprovalScope::Global, .. }));
        assert!(matches!(pick("deny"), ApprovalDecision::Deny { .. }));
        assert!(matches!(pick("other"), ApprovalDecision::Deny { .. }));
        assert!(matches!(
            approval_decision(&json!({"outcome": {"outcome": "cancelled"}})),
            ApprovalDecision::Deny { .. }
        ));
    }

    #[test]
    fn questions_become_a_form_and_the_answers_come_back() {
        let qs = vec![q("Which?", &["A", "B"], false), q("Which ones?", &["X", "Y", "Z"], true)];
        let p = elicitation_params("s1", &qs);
        assert_eq!((p["mode"].as_str(), p["sessionId"].as_str()), (Some("form"), Some("s1")));
        let props = &p["requestedSchema"]["properties"];
        assert_eq!(props["q0"]["oneOf"][1]["const"], "B");
        assert_eq!(props["q0"]["default"], "B", "the recommended option is preselected");
        assert_eq!(props["q1"]["type"], "array");
        assert_eq!(props["q1"]["items"]["anyOf"][2]["const"], "Z");
        assert!(props["q1"].get("default").is_none());
        let got = answers_from(&json!({"q0": "A", "q1": ["X", "Z"]}), &qs);
        assert!(matches!(&got[0], Answer::Options(o) if o == &["A"]));
        assert!(matches!(&got[1], Answer::Options(o) if o == &["X", "Z"]));
        let got = answers_from(&json!({"q0": "A", "q0_other": " mine ", "q1": []}), &qs);
        assert!(matches!(&got[0], Answer::Custom(t) if t == "mine"), "a typed answer wins");
        assert!(matches!(got[1], Answer::Cancelled), "nothing chosen is no answer");
        let skippable: Vec<AskQuestion> = qs.iter().cloned().map(|q| AskQuestion { skippable: true, ..q }).collect();
        let got = answers_from(&json!({"q0": "A"}), &skippable);
        assert!(matches!(got[1], Answer::Skipped), "a skippable question left empty is skipped");
    }

    #[test]
    fn a_declined_form_answers_nothing() {
        let rpc_out = Rc::new(RefCell::new(Vec::<Value>::new()));
        let log = rpc_out.clone();
        let rpc = Rpc::new(Rc::new(move |v: &Value| log.borrow_mut().push(v.clone())));
        let (tx, mut rx) = oneshot::channel();
        mu_testutil::block_on_local(async {
            let task =
                tokio::task::spawn_local(ask_user(rpc.clone(), "s".into(), vec![q("Which?", &["A"], false)], tx));
            tokio::task::yield_now().await;
            assert!(rpc.resolve(&json!(1), Ok(json!({"action": "decline"}))));
            task.await.unwrap();
        });
        assert_eq!(rpc_out.borrow()[0]["method"], "elicitation/create");
        assert!(rx.try_recv().unwrap().is_none());
    }
}
