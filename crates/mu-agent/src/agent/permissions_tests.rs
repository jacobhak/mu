//! The gate: what prompts in each mode, and what a decision does to the call and the turn.

use super::*;
use crate::agent::RunConfig;
use crate::testing::{agent_run, assistant};
use mu_base::config::Configs;
use mu_base::permissions::{ApprovalDecision, ApprovalRequest, ApprovalScope};
use mu_testutil::block_on_local as run;

fn ask() -> RunConfig {
    RunConfig { permission_mode: Some(PermissionMode::Ask), ..RunConfig::default() }
}

fn bash_call(id: &str, cmd: &str) -> Block {
    Block::ToolCall { id: id.into(), name: "bash".into(), arguments: json!({"command": cmd}) }
}

fn tool_texts(a: &Agent) -> Vec<String> {
    a.messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { content, .. } => Some(text_of(content)),
            _ => None,
        })
        .collect()
}

/// Runs one turn while a task answers every approval with `answer`.
async fn turn_answered(
    a: &mut Agent,
    mut rx: UnboundedReceiver<AgentEvent>,
    text: &str,
    answer: impl Fn(&ApprovalRequest) -> ApprovalDecision + 'static,
) -> Vec<ApprovalRequest> {
    let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
    let log = seen.clone();
    let task = tokio::task::spawn_local(async move {
        while let Some(e) = rx.recv().await {
            match e {
                AgentEvent::Approve { request, reply } => {
                    log.borrow_mut().push(request.clone());
                    let _ = reply.send(answer(&request));
                }
                AgentEvent::TurnEnd { .. } => break,
                _ => {}
            }
        }
    });
    a.turn(Some(text.to_string().into())).await;
    task.await.unwrap();
    seen.borrow().clone()
}

#[test]
fn a_deny_with_a_note_does_not_run_the_call_and_the_turn_goes_on() {
    run(async {
        let script = vec![
            Ok(assistant(vec![bash_call("t1", "touch permission-test-a")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent_run(script, Configs::default(), ask());
        let reqs = turn_answered(&mut a, rx, "go", |_| ApprovalDecision::Deny { note: Some("no".into()) }).await;
        assert_eq!(reqs.len(), 1, "one prompt for the exec-tier call");
        assert_eq!(reqs[0].title, "bash: touch permission-test-a");
        assert_eq!(tool_texts(&a), ["Not run: the user denied this bash call: no"]);
        let ended = a.messages.iter().filter(|m| matches!(m, Message::Assistant { .. })).count();
        assert_eq!(ended, 2, "the model replied after the denial");
    });
}

#[test]
fn a_session_grant_covers_later_calls() {
    run(async {
        let script = vec![
            Ok(assistant(vec![bash_call("t1", "cargo test")], StopReason::ToolUse)),
            Ok(assistant(vec![bash_call("t2", "cargo test --x")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent_run(script, Configs::default(), ask());
        let reqs =
            turn_answered(&mut a, rx, "go", |_| ApprovalDecision::Allow { scope: ApprovalScope::Session, note: None })
                .await;
        assert_eq!(reqs.len(), 1, "the second call is covered by the session grant");
        assert_eq!(reqs[0].grant.as_ref().unwrap().patterns, ["cargo test *"]);
        assert_eq!(tool_texts(&a).len(), 2);
    });
}

#[test]
fn read_only_commands_do_not_prompt() {
    run(async {
        let script = vec![
            Ok(assistant(vec![bash_call("t1", "ls")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent_run(script, Configs::default(), ask());
        let reqs = turn_answered(&mut a, rx, "go", |_| panic!("no prompt expected")).await;
        assert!(reqs.is_empty());
        assert_eq!(tool_texts(&a).len(), 1);
    });
}

#[test]
fn esc_denies_and_ends_the_turn() {
    run(async {
        let script = vec![
            Ok(assistant(vec![bash_call("t1", "touch permission-test-b")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = agent_run(script, Configs::default(), ask());
        let answer = tokio::task::spawn_local(async move {
            while let Some(e) = rx.recv().await {
                match e {
                    AgentEvent::Approve { reply, .. } => {
                        let _ = reply.send(ApprovalDecision::Deny { note: None });
                    }
                    AgentEvent::TurnEnd { aborted } => return aborted,
                    _ => {}
                }
            }
            false
        });
        a.turn(Some("go".to_string().into())).await;
        assert!(answer.await.unwrap(), "the turn was cancelled");
        assert_eq!(tool_texts(&a), ["Not run: the user denied this call."]);
        assert!(!std::path::Path::new("/tmp/permission-test-b").exists());
    });
}

#[test]
fn a_deny_rule_never_prompts() {
    run(async {
        let script = vec![
            Ok(assistant(vec![bash_call("t1", "git push")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let run_cfg = RunConfig {
            permission_mode: Some(PermissionMode::Yolo),
            inherited_permissions: Some(json!({
                "mode": "yolo",
                "session": [{"tool": "bash", "pattern": "git push *", "action": "deny"}],
            })),
            ..Default::default()
        };
        let (mut a, rx) = agent_run(script, Configs::default(), run_cfg);
        let reqs = turn_answered(&mut a, rx, "go", |_| panic!("no prompt expected")).await;
        assert!(reqs.is_empty());
        assert_eq!(tool_texts(&a), ["Not run: permission rule \"git push *\" = \"deny\" for bash (this session)."]);
    });
}
