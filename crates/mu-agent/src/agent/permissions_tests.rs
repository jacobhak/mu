//! The gate: what prompts in each mode, and what a decision does to the call and the turn.

use super::*;
use crate::agent::RunConfig;
use crate::testing::{agent_in, agent_run, assistant};
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

/// A directory outside the checkout, held at its resolved path. macOS resolves `/etc` and `/tmp`
/// through symlinks and the gate matches subjects as resolved, so the platform's own directories
/// cannot serve as fixtures.
fn outside(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mu-perm-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
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
        // The calls really run, so the command has to stay cheap: a `cargo test` here built and ran
        // the suite inside this test.
        let script = vec![
            Ok(assistant(vec![bash_call("t1", "cargo tree")], StopReason::ToolUse)),
            Ok(assistant(vec![bash_call("t2", "cargo tree --depth 1")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent_run(script, Configs::default(), ask());
        let reqs = turn_answered(&mut a, rx, "go", |_| ApprovalDecision::Allow {
            scope: ApprovalScope::Session,
            note: None,
            patterns: None,
        })
        .await;
        assert_eq!(reqs.len(), 1, "the second call is covered by the session grant");
        assert_eq!(reqs[0].grant.as_ref().unwrap().patterns, ["cargo tree *"]);
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
fn a_read_outside_the_project_prompts_and_is_granted_by_its_path() {
    run(async {
        let outside = outside("read-outside");
        let path = outside.join("passwd").to_string_lossy().into_owned();
        let call = Block::ToolCall { id: "t1".into(), name: "read".into(), arguments: json!({"path": &path}) };
        let script = vec![
            Ok(assistant(vec![call], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let root = mu_testutil::repo();
        let (mut a, rx) = agent_in(script, Configs::default(), ask(), &root);
        let reqs = turn_answered(&mut a, rx, "go", |_| ApprovalDecision::Deny { note: Some("not that".into()) }).await;
        assert_eq!(reqs.len(), 1, "reading outside the project prompts");
        assert_eq!(reqs[0].title, format!("read: {path}"));
        assert_eq!(reqs[0].reason, "ask mode: reading outside the project needs approval");
        assert_eq!(reqs[0].grant.as_ref().unwrap().patterns, [path]);
        assert_eq!(tool_texts(&a), ["Not run: the user denied this read call: not that"]);
        std::fs::remove_dir_all(&outside).ok();
    });
}

#[test]
fn a_wider_answer_covers_a_neighbouring_file() {
    run(async {
        let outside = outside("wider");
        let dir = outside.to_string_lossy().into_owned();
        let (first, second) = (outside.join("passwd"), outside.join("hosts"));
        let call = |path: String, id: &str| Block::ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({"path": path}),
        };
        let script = vec![
            Ok(assistant(vec![call(first.to_string_lossy().into_owned(), "t1")], StopReason::ToolUse)),
            Ok(assistant(vec![call(second.to_string_lossy().into_owned(), "t2")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let root = mu_testutil::repo();
        let (mut a, rx) = agent_in(script, Configs::default(), ask(), &root);
        let width = format!("{dir}/*");
        let answer = width.clone();
        let reqs = turn_answered(&mut a, rx, "go", move |_| ApprovalDecision::Allow {
            scope: ApprovalScope::Session,
            note: None,
            patterns: Some(vec![answer.clone()]),
        })
        .await;
        assert_eq!(reqs.len(), 1, "the second file is covered by the directory grant");
        assert_eq!(reqs[0].grant.as_ref().unwrap().wider[0].patterns, [width]);
        assert_eq!(tool_texts(&a).len(), 2, "both reads ran");
        std::fs::remove_dir_all(&outside).ok();
    });
}

#[test]
fn a_granted_directory_covers_grep_too() {
    run(async {
        let outside = outside("granted-dir");
        let dir = outside.to_string_lossy().into_owned();
        let (passwd, hosts) = (outside.join("passwd"), outside.join("hosts"));
        let script = vec![
            Ok(assistant(
                vec![Block::ToolCall {
                    id: "t1".into(),
                    name: "read".into(),
                    arguments: json!({"path": passwd.to_string_lossy()}),
                }],
                StopReason::ToolUse,
            )),
            Ok(assistant(
                vec![Block::ToolCall {
                    id: "t2".into(),
                    name: "grep".into(),
                    arguments: json!({"pattern": "x", "path": hosts.to_string_lossy()}),
                }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let root = mu_testutil::repo();
        let (mut a, rx) = agent_in(script, Configs::default(), ask(), &root);
        let reqs = turn_answered(&mut a, rx, "go", move |_| ApprovalDecision::Allow {
            scope: ApprovalScope::Session,
            note: None,
            patterns: Some(vec![format!("{dir}/*")]),
        })
        .await;
        assert_eq!(reqs.len(), 1, "the grant covers grep of the same directory");
        assert_eq!(tool_texts(&a).len(), 2, "both calls ran");
        std::fs::remove_dir_all(&outside).ok();
    });
}

#[test]
fn a_write_grant_covers_editing_the_same_file() {
    run(async {
        let root = mu_testutil::repo();
        let script = vec![
            Ok(assistant(
                vec![Block::ToolCall {
                    id: "t1".into(),
                    name: "write".into(),
                    arguments: json!({"path": "c.txt", "content": "x\n"}),
                }],
                StopReason::ToolUse,
            )),
            Ok(assistant(
                vec![Block::ToolCall {
                    id: "t2".into(),
                    name: "edit".into(),
                    // The write's result names the file as `[c.txt#9396]`, a hash of the content it wrote.
                    arguments: json!({"input": "[c.txt#9396]\nPUT 1.=1:\n+y\n"}),
                }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent_in(script, Configs::default(), ask(), &root);
        let reqs = turn_answered(&mut a, rx, "go", |_| ApprovalDecision::Allow {
            scope: ApprovalScope::Session,
            note: None,
            patterns: None,
        })
        .await;
        assert_eq!(reqs.len(), 1, "the write's grant covers editing that file");
        assert_eq!(tool_texts(&a).len(), 2, "both calls ran");
        assert_eq!(std::fs::read_to_string(root.join("c.txt")).unwrap(), "y\n");
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
