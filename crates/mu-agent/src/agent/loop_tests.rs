use super::*;
use crate::agent::RunConfig;
use crate::testing::{
    agent, agent_in, agent_run, agent_with, assistant, call, drain, git_repo, results, save_plan_in, tmp_dir,
    yield_call,
};
use mu_base::config::Configs;
use mu_base::permissions::{ApprovalDecision, ApprovalScope};
use mu_provider::provider::types::ProviderError;
use mu_session::session::inbox;
use mu_session::session::inbox::enqueue;
use mu_testutil::block_on_local as run;

#[test]
fn tool_panic_becomes_an_error_result() {
    run(async {
        let script = vec![
            Ok(assistant(vec![call("t1", "__panic")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, _rx) = agent(script);
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        let err = a.messages.iter().find_map(|m| match m {
            Message::ToolResult { is_error: true, content, .. } => Some(format!("{content:?}")),
            _ => None,
        });
        assert!(err.unwrap().contains("tool panicked: boom"));
        assert_eq!(a.messages.len(), 4, "the turn continued after the panic");
    });
}

#[test]
fn secrets_are_replaced_explained_and_never_written_back() {
    run(async {
        let tok = format!("ghp_{}", "a1B2".repeat(9));
        // The command prints the token without holding it whole.
        let print = json!({"command": format!("printf '%s%s' ghp_ {}", &tok[4..])});
        let write_back = json!({"command": "echo [REDACTED:github-token] > f"});
        let script = vec![
            Ok(assistant(
                vec![Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: print }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
            Ok(assistant(
                vec![Block::ToolCall { id: "t2".into(), name: "bash".into(), arguments: write_back }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, _rx) = agent(script);
        a.turn(Some("go".to_string().into())).await;
        a.turn(Some(format!("mine is {tok}").into())).await;
        let texts: Vec<(bool, String)> = a
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult { content, is_error, .. } => Some((*is_error, text_of(content))),
                Message::User { content, .. } => Some((false, text_of(content))),
                _ => None,
            })
            .collect();
        assert!(texts.iter().all(|(_, t)| !t.contains(&tok)), "{texts:?}");
        let printed = &texts[1].1;
        assert!(printed.starts_with("[REDACTED:github-token]"), "{printed}");
        assert!(printed.contains("(recognised key format)"), "the model is told: {printed}");
        assert_eq!(texts[2].1, "mine is [REDACTED:github-token]");
        let (refused, why) = &texts[3];
        assert!(*refused && why.contains("placeholder"), "{why}");
        // Each replacement is logged by name, never by value.
        let file = std::fs::read_to_string(&a.session.as_ref().unwrap().path).unwrap();
        assert!(!file.contains(&tok));
        let logged: Vec<Value> = file
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["customType"] == "secrets_redacted")
            .map(|v| v["data"]["at"].clone())
            .collect();
        assert_eq!(logged, [json!("tool:bash"), json!("message")]);
    });
}

#[test]
fn a_secret_in_a_context_file_does_not_reach_the_system_prompt() {
    run(async {
        let tok = format!("ghp_{}", "c3D4".repeat(9));
        let (mut a, _rx) = agent(vec![]);
        let files = &mut Rc::get_mut(&mut a.ctx).expect("sole owner").files;
        files.push(mu_tools::context::ContextFile { path: "AGENTS.md".into(), content: format!("deploy with {tok}") });
        let sp = a.system();
        assert!(sp.contains("deploy with [REDACTED:github-token]"), "{sp}");
        assert!(!sp.contains(&tok));
    });
}

#[test]
fn truncated_reply_with_calls_stays_paired() {
    run(async {
        let script = vec![
            Ok(assistant(vec![call("t1", "read")], StopReason::Length)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, _rx) = agent(script);
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        a.turn(Some("again".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        assert!(a.messages.iter().any(|m| matches!(m, Message::ToolResult { is_error: true, .. })));
    });
}

#[test]
fn plan_prompt_inlines_plan_and_path() {
    let p = ExecPlan {
        content: "do it\n".into(),
        title: "T".into(),
        uri: "local://t-plan.md".into(),
        decision: ModeAction::Compact { model: None },
        mode: "plan".into(),
        base: None,
        approved_at: 0,
        stored: None,
    };
    let m = &mu_tools::context::builtin_modes()[0];
    let s = plan_prompt(&p, m);
    assert!(s.contains("<plan path=\"local://t-plan.md\">\ndo it\n</plan>") && s.contains("History usable"));
    assert!(!s.contains("{{"));
    let p = ExecPlan { content: "uses {{file}} and {{tool}}\n".into(), ..p };
    assert!(plan_prompt(&p, m).contains("uses {{file}} and {{tool}}"), "the document is not templated");
}

#[test]
fn failed_turn_clears_stale_exec_plan() {
    run(async {
        let (mut a, _rx) = agent(vec![Err(ProviderError::Fatal("nope".into()))]);
        a.plan.exec_plan = Some(ExecPlan {
            content: "plan".into(),
            title: String::new(),
            uri: String::new(),
            decision: ModeAction::Execute { model: None },
            mode: "plan".into(),
            base: None,
            approved_at: 0,
            stored: None,
        });
        a.turn(Some("go".to_string().into())).await;
        assert!(a.plan.exec_plan.is_none());
    });
}

#[test]
fn policy_is_enforced_at_dispatch_and_in_defs() {
    run(async {
        let script = vec![
            Ok(assistant(vec![call("t1", "bash")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (base, _rx0) = agent(vec![]);
        assert!(base.tool_defs().iter().any(|d| d.name == "ask"));
        let (mut a, _rx) = agent_run(
            script,
            Configs::default(),
            RunConfig { allow: Some(vec!["read".into()]), excluded: vec!["ask"], ..Default::default() },
        );
        assert!(a.tool_defs().iter().all(|d| d.name == "read"));
        a.turn(Some("go".to_string().into())).await;
        let err = a.messages.iter().find_map(|m| match m {
            Message::ToolResult { is_error: true, content, .. } => Some(format!("{content:?}")),
            _ => None,
        });
        assert!(err.unwrap().contains("not allowed"));
    });
}

#[test]
fn job_cancel_is_offered_where_jobs_exist_and_stops_a_background_bash_job() {
    run(async {
        let (base, _rx) = agent(vec![]);
        assert!(base.tool_defs().iter().all(|d| d.name != "job_cancel"), "no jobs, no tool");
        let (mut a, _rx) = notify_agent(vec![]);
        assert!(a.tool_defs().iter().any(|d| d.name == "job_cancel"));

        let cancel = CancellationToken::new();
        let started = a.run_tool("b1", "bash", json!({"command": "sleep 30", "async": true}), &cancel).await;
        assert!(!started.is_error, "{}", started.text);
        let jobs = a.tools.jobs.clone().unwrap();
        let id = jobs.running_ids().remove(0);
        let out = a.run_tool("c1", "job_cancel", json!({"id": id}), &cancel).await;
        assert!(!out.is_error && out.text.contains("Cancelled"), "{}", out.text);
        mu_testutil::wait_until("the cancelled job's result", || jobs.has_undelivered()).await;
        let done = jobs.take_undelivered();
        assert!(done.len() == 1 && !done[0].ok, "the job ended, cancelled, long before 30 s");
        assert!(jobs.running_ids().is_empty());
    });
}

#[test]
fn calls_after_a_successful_yield_are_skipped() {
    run(async {
        let script = vec![
            Ok(assistant(vec![yield_call("y1", json!({"data":{"n":1}})), call("t2", "__panic")], StopReason::ToolUse)),
            Err(ProviderError::Fatal("requested after a successful yield".into())),
        ];
        let (mut a, mut rx) = agent(script);
        *a.tools.output_schema.borrow_mut() =
            Some(json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}));
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        let r = results(&a);
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(!r[0].0 && r[0].1.contains("Result submitted"), "{:?}", r[0]);
        assert!(r[1].0 && r[1].1.contains("already submitted with `yield`"), "{:?}", r[1]);
        let mut skipped = Vec::new();
        let mut ended = None;
        while let Ok(e) = rx.try_recv() {
            match e {
                AgentEvent::TurnEnd { aborted } => ended = Some(aborted),
                AgentEvent::ToolSkipped { id } => skipped.push(id),
                _ => {}
            }
        }
        assert_eq!(skipped, ["t2"]);
        assert_eq!(ended, Some(false));
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1);
    });
}

#[test]
fn stop_skips_the_rest_of_the_batch_and_ends_the_turn() {
    run(async {
        let script = vec![
            Ok(assistant(vec![call("t1", "__panic"), call("t2", "__panic")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = agent(script);
        a.ports.inbox.borrow_mut().stop = true;
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        let r = results(&a);
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x.0 && x.1.contains("the run was stopped")), "{r:?}");
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no second request");
        let mut ended = None;
        let mut skipped = Vec::new();
        while let Ok(e) = rx.try_recv() {
            match e {
                AgentEvent::TurnEnd { aborted } => ended = Some(aborted),
                AgentEvent::ToolSkipped { id } => skipped.push(id),
                _ => {}
            }
        }
        assert_eq!(ended, Some(false));
        assert_eq!(skipped, ["t1", "t2"], "each skipped call is marked structurally");
    });
}

#[test]
fn stop_during_a_tool_lets_it_finish_and_skips_the_next() {
    run(async {
        let started_dir = mu_testutil::tmp();
        let started = started_dir.join("started");
        let release = started_dir.join("release");
        let command = format!("touch {}; while [ ! -f {} ]; do sleep 0.02; done", started.display(), release.display());
        let sleep = Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": command}) };
        let script = vec![
            Ok(assistant(vec![sleep, call("t2", "__panic")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = agent(script);
        let inbox = a.ports.inbox.clone();
        tokio::task::spawn_local(async move {
            mu_testutil::wait_until("the tool to start", || started.exists()).await;
            inbox.borrow_mut().stop = true;
            std::fs::write(&release, "").unwrap();
        });
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        let r = results(&a);
        assert_eq!(r.len(), 2);
        assert!(!r[0].0, "the running tool finished: {:?}", r[0]);
        assert!(r[1].0 && r[1].1.contains("the run was stopped"), "{:?}", r[1]);
        let mut skipped = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let AgentEvent::ToolSkipped { id } = e {
                skipped.push(id);
            }
        }
        assert_eq!(skipped, ["t2"], "the call that ran is not marked");
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no second request");
    });
}

#[test]
fn stop_with_a_plain_reply_ends_normally() {
    run(async {
        let (mut a, _rx) = agent(vec![Ok(assistant(vec![Block::text("hi")], StopReason::Stop))]);
        a.ports.inbox.borrow_mut().stop = true;
        a.turn(Some("go".to_string().into())).await;
        assert!(matches!(a.messages.last(), Some(Message::Assistant { .. })));
        assert!(a.ports.providers.scripted.borrow().is_empty());
    });
}

#[test]
fn stop_plus_cancel_is_an_abort_and_stays_paired() {
    run(async {
        let started_dir = mu_testutil::tmp();
        let started = started_dir.join("started");
        let command = format!("touch {}; sleep 600", started.display());
        let sleep = Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": command}) };
        let script = vec![Ok(assistant(vec![sleep, call("t2", "__panic")], StopReason::ToolUse))];
        let (mut a, mut rx) = agent(script);
        let inbox = a.ports.inbox.clone();
        tokio::task::spawn_local(async move {
            mu_testutil::wait_until("the tool to start", || started.exists()).await;
            inbox.borrow_mut().stop = true;
            tokio::task::yield_now().await;
            inbox.borrow().cancel.cancel();
        });
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        let mut ended = None;
        while let Ok(e) = rx.try_recv() {
            if let AgentEvent::TurnEnd { aborted } = e {
                ended = Some(aborted);
            }
        }
        assert_eq!(ended, Some(true));
    });
}

#[test]
fn kill_jobs_is_handled_mid_turn_and_other_commands_queue() {
    run(async {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Command::KillJobs(None)).unwrap();
        tx.send(Command::NewSession).unwrap();
        let live = Rc::new(std::cell::Cell::new(Thinking::Off));
        let mut q = Queue {
            rx,
            inbox: new_inbox(),
            backlog: Default::default(),
            jobs: None,
            events: etx,
            live,
            perms: Rc::default(),
        };
        drive(crate::testing::long_turn(), &mut q).await;
        assert!(matches!(erx.try_recv(), Ok(AgentEvent::Notice(m)) if m.contains("killed")));
        assert_eq!(q.backlog.len(), 1);
        assert!(matches!(q.backlog.pop_front(), Some(Command::NewSession)));
    });
}

#[test]
fn a_fatal_error_emits_turn_failed_before_turn_end() {
    run(async {
        let (mut a, mut rx) = agent(vec![Err(ProviderError::Fatal("nope".into()))]);
        a.turn(Some("go".to_string().into())).await;
        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            match e {
                AgentEvent::Error(_) => seen.push("error"),
                AgentEvent::TurnFailed => seen.push("failed"),
                AgentEvent::TurnEnd { .. } => seen.push("end"),
                _ => {}
            }
        }
        assert_eq!(seen, ["error", "failed", "end"]);
    });
}

/// Whether a `/retry` only said there was nothing to resend.
fn is_nothing_to_retry(events: &[AgentEvent]) -> bool {
    matches!(events, [AgentEvent::Notice(m)] if m == "nothing to retry")
}

#[test]
fn retry_resends_the_unanswered_request_without_recording_the_prompt() {
    run(async {
        let script =
            vec![Err(ProviderError::Fatal("nope".into())), Ok(assistant(vec![Block::text("done")], StopReason::Stop))];
        let (mut a, mut rx) = agent(script);
        a.turn(Some("go".to_string().into())).await;
        assert_eq!(a.messages.len(), 1, "the prompt is kept, the failure is not");
        drain(&mut rx);
        a.retry().await;
        let kept: Vec<&Message> = a.messages.iter().collect();
        assert_eq!(kept.len(), 2, "only the reply is added: {kept:?}");
        assert!(matches!(kept[0], Message::User { .. }) && matches!(kept[1], Message::Assistant { .. }));
        let events = drain(&mut rx);
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::UserPrompt(_))), "the prompt is echoed again");
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnStart)));
    });
}

#[test]
fn retry_with_nothing_unanswered_starts_nothing() {
    run(async {
        let (mut a, mut rx) = agent(vec![Ok(assistant(vec![Block::text("done")], StopReason::Stop))]);
        a.retry().await;
        assert!(is_nothing_to_retry(&drain(&mut rx)), "an empty conversation has nothing to resend");
        assert!(a.messages.is_empty());
        a.turn(Some("go".to_string().into())).await;
        drain(&mut rx);
        a.retry().await;
        assert!(is_nothing_to_retry(&drain(&mut rx)), "a finished turn has nothing to resend");
        assert_eq!(a.messages.len(), 2);
    });
}

#[test]
fn a_turn_with_no_model_chosen_sends_nothing_and_keeps_no_message() {
    run(async {
        let (mut a, mut rx) = agent(vec![]);
        a.model.model = mu_base::models::ModelSpec::unselected();
        a.turn(Some("go".to_string().into())).await;
        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            match e {
                AgentEvent::Error(m) => seen.push(m),
                AgentEvent::TurnEnd { aborted } => seen.push(format!("end {aborted}")),
                _ => {}
            }
        }
        assert!(seen[0].contains("no model chosen") && seen[1] == "end true", "{seen:?}");
        assert!(a.messages.is_empty());
        assert_eq!(a.snapshot().model_name, "no model");
    });
}

// ---- `mu notify` delivery ----

fn notify_agent(script: Vec<Result<Message, ProviderError>>) -> (Agent, UnboundedReceiver<AgentEvent>) {
    let (mut a, rx) = agent(script);
    a.enable_jobs();
    a.enable_notify();
    (a, rx)
}

fn user_text(m: &Message) -> Option<String> {
    match m {
        Message::User { content, .. } => Some(text_of(content)),
        _ => None,
    }
}

fn user_texts(a: &Agent) -> Vec<String> {
    a.messages.iter().filter_map(user_text).collect()
}

fn session_text(a: &Agent) -> String {
    std::fs::read_to_string(&a.session.as_ref().unwrap().path).unwrap()
}

#[test]
fn a_wake_with_nothing_pending_starts_no_turn() {
    run(async {
        let (mut a, mut rx) = notify_agent(vec![]);
        a.ensure_session().await;
        drain(&mut rx);
        a.turn(None).await;
        assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::TurnStart | AgentEvent::Woken)));
    });
}

#[test]
fn a_stop_with_no_calls_skips_the_todo_reminder() {
    use mu_tools::tools::todo::{Phase, Status, Task};
    run(async {
        let script = vec![
            Ok(assistant(vec![Block::text("hi")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ];
        let (mut a, _rx) = agent(script);
        let task = Task { content: "open item".into(), status: Status::Pending, blocker: None };
        a.tools.set_todo(vec![Phase { name: "p".into(), tasks: vec![task] }]);
        a.ports.inbox.borrow_mut().stop = true;
        a.turn(Some("go".to_string().into())).await;
        assert_eq!(a.messages.len(), 2, "no reminder was pushed: {:?}", a.messages.len());
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no second request");
    });
}

#[test]
fn the_todo_reminder_waits_for_a_running_background_job() {
    use mu_tools::tools::subagent::BackgroundResult;
    use mu_tools::tools::todo::{Phase, Status, Task};
    run(async {
        let script = vec![
            Ok(assistant(vec![Block::text("waiting on bg")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ];
        let (mut a, _rx) = notify_agent(script);
        let task = Task { content: "open item".into(), status: Status::Pending, blocker: None };
        a.tools.set_todo(vec![Phase { name: "p".into(), tasks: vec![task] }]);
        let jobs = a.tools.jobs.clone().unwrap();
        jobs.spawn_bash("sleep 99".into(), std::env::temp_dir(), |cancel, _| async move {
            cancel.cancelled().await;
            BackgroundResult { ok: false, text: String::new(), note: "cancelled".into(), meta: None }
        });
        assert_eq!(jobs.running(), 1);
        a.turn(Some("go".to_string().into())).await;
        assert_eq!(a.messages.len(), 2, "no reminder while the job runs");
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no second request");
        jobs.kill(None);
    });
}

#[test]
fn a_sleeping_command_runs_while_a_job_runs_instead_of_being_refused() {
    use mu_tools::tools::subagent::BackgroundResult;
    run(async {
        let script = vec![
            Ok(assistant(
                vec![Block::ToolCall {
                    id: "w1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "echo waiting"}),
                }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = notify_agent(script);
        let jobs = a.tools.jobs.clone().unwrap();
        jobs.spawn_bash("cargo build".into(), std::env::temp_dir(), |cancel, _| async move {
            cancel.cancelled().await;
            BackgroundResult { ok: false, text: String::new(), note: "cancelled".into(), meta: None }
        });
        a.turn(Some("go".to_string().into())).await;
        jobs.kill(None);
        let r = results(&a);
        assert!(!r[0].0 && r[0].1.contains("text: \"waiting"), "the command runs: {:?}", r[0]);
        assert!(!drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::ToolSkipped { id } if id == "w1")));
        assert!(a.ports.providers.scripted.borrow().is_empty(), "the turn continues after the call");
    });
}

#[test]
fn a_wait_with_no_job_running_runs() {
    run(async {
        let script = vec![
            Ok(assistant(
                vec![Block::ToolCall {
                    id: "w1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "echo waiting"}),
                }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, _rx) = notify_agent(script);
        a.turn(Some("go".to_string().into())).await;
        let r = results(&a);
        assert!(!r[0].0 && r[0].1.contains("text: \"waiting"), "{:?}", r[0]);
        assert!(!r[0].1.contains("<background-jobs>"), "{:?}", r[0]);
        assert!(a.ports.providers.scripted.borrow().is_empty());
    });
}

#[test]
fn delegated_roles_reach_the_top_level_prompt_and_task_schema_only() {
    run(async {
        let cfg = || {
            Configs::from_models_toml(
                "[sets.main]\ndefault = \"a/b\"\nw = \"a/c\"\n[roles.w]\ndescription = \"Docs\"\ndelegate = true\n",
            )
        };
        let task_model = |a: &Agent| {
            a.tool_defs().into_iter().find(|d| d.name == "task").and_then(|d| {
                d.schema["properties"]["tasks"]["items"]["properties"]["model"]["enum"].as_array().cloned()
            })
        };
        let (parent, _rx) = agent_with(vec![], cfg());
        assert!(parent.system().contains("  - `@w`: Docs\n"), "{}", parent.system());
        assert_eq!(task_model(&parent), Some(vec![json!("@w")]));
        assert_eq!(*parent.tools.delegated.borrow(), ["@w"]);

        let (sub, _rx) = agent_run(vec![], cfg(), RunConfig { subagent: true, ..Default::default() });
        assert!(!sub.system().contains("- `@w`:"));
        assert_eq!(task_model(&sub), None);

        let no_delegate = Configs::from_models_toml(
            "[sets.main]\ndefault = \"a/b\"\nw = \"a/c\"\n[roles.w]\ndescription = \"Docs\"\n",
        );
        let (plain, _rx) = agent_with(vec![], no_delegate);
        assert!(!plain.system().contains("- `@w`:"));
        assert_eq!(task_model(&plain), None);
    });
}

fn bash_call(id: &str, args: serde_json::Value) -> Block {
    Block::ToolCall { id: id.into(), name: "bash".into(), arguments: args }
}

#[test]
fn wait_is_offered_only_with_jobs() {
    run(async {
        let names = |a: &Agent| a.tool_defs().into_iter().map(|d| d.name).collect::<Vec<_>>();
        let bash = |a: &Agent| a.tool_defs().into_iter().find(|d| d.name == "bash").unwrap().description;

        let (headless, _rx) = agent(vec![]);
        assert!(!names(&headless).contains(&"wait".to_string()), "no jobs, no wait");
        assert!(bash(&headless).contains("The shell persists across calls"), "no jobs keeps the persistent shell");

        let (mut interactive, _rx) = agent(vec![]);
        interactive.enable_jobs();
        assert!(names(&interactive).contains(&"wait".to_string()));
        assert!(bash(&interactive).contains("Each call runs in its own shell"), "jobs describe isolated calls");
        assert!(!bash(&interactive).contains("Never wait for a job in the foreground"));
    });
}

#[test]
fn a_waiting_command_runs_instead_of_being_refused() {
    use mu_tools::tools::subagent::BackgroundResult;
    run(async {
        let script = vec![
            Ok(assistant(vec![bash_call("w1", json!({"command": "printf 'waited\\n'"}))], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, _rx) = agent(script);
        a.enable_jobs();
        let jobs = a.tools.jobs.clone().unwrap();
        jobs.spawn_bash("cargo build".into(), std::env::temp_dir(), |cancel, _| async move {
            cancel.cancelled().await;
            BackgroundResult { ok: false, text: String::new(), note: "cancelled".into(), meta: None }
        });
        a.turn(Some("go".to_string().into())).await;
        jobs.kill(None);
        let r = results(&a);
        assert!(!r[0].0 && r[0].1.contains("waited"), "the command ran instead of being refused: {:?}", r[0]);
        assert!(r[0].1.contains("use `wait` when you need it now"), "{:?}", r[0]);
        assert!(!r[0].1.contains("end your turn now"), "{:?}", r[0]);
    });
}

#[test]
fn a_running_job_is_reported_on_the_last_result_of_a_later_batch() {
    use mu_tools::tools::subagent::BackgroundResult;
    run(async {
        let bash = |id: &str, cmd: &str| Block::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: json!({"command": cmd}),
        };
        let script = vec![
            Ok(assistant(vec![bash("a", "basename /x/a"), bash("b", "basename /x/b")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, _rx) = notify_agent(script);
        let jobs = a.tools.jobs.clone().unwrap();
        jobs.spawn_bash("cargo build".into(), std::env::temp_dir(), |cancel, _| async move {
            cancel.cancelled().await;
            BackgroundResult { ok: false, text: String::new(), note: "cancelled".into(), meta: None }
        });
        a.turn(Some("go".to_string().into())).await;
        jobs.kill(None);
        let r = results(&a);
        assert!(!r[0].1.contains("<background-jobs>"), "{:?}", r[0]);
        assert!(r[1].1.contains("<background-jobs>\\nbg_1 (bash: cargo build) has been running for"), "{:?}", r[1]);
        assert!(r[1].1.contains("mu automatically starts your next turn"), "{:?}", r[1]);
    });
}

#[test]
fn a_job_started_in_the_batch_gets_no_status_block() {
    run(async {
        let script = vec![
            Ok(assistant(
                vec![Block::ToolCall {
                    id: "s".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "sleep 30", "async": true}),
                }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, _rx) = notify_agent(script);
        let jobs = a.tools.jobs.clone().unwrap();
        a.turn(Some("go".to_string().into())).await;
        jobs.kill(None);
        let r = results(&a);
        assert!(r[0].1.contains("Started background job"), "{:?}", r[0]);
        assert!(!r[0].1.contains("<background-jobs>"), "{:?}", r[0]);
    });
}

/// A turn that ends with a prose question, with `tweak` applied first. Returns the message count
/// and how many scripted replies were left unused.
async fn question_turn(run: RunConfig) -> (usize, usize) {
    let (mut a, _rx) = agent_run(
        vec![
            Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ],
        Configs::default(),
        run,
    );
    a.turn(Some("go".to_string().into())).await;
    let left = a.ports.providers.scripted.borrow().len();
    (a.messages.len(), left)
}

#[test]
fn ask_nudge_fires_once_then_the_loop_continues() {
    run(async {
        let (mut a, _rx) = agent(vec![
            Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text(".")], StopReason::Stop)),
        ]);
        a.turn(Some("go".to_string().into())).await;
        assert_eq!(a.messages.len(), 4);
        assert!(
            matches!(&a.messages[2], Message::User { content, .. } if text_of(content) == mu_base::ask_nudge::REMINDER)
        );
        assert!(a.ports.providers.scripted.borrow().is_empty(), "the loop continued once");
    });
}

#[test]
fn ask_nudge_is_not_repeated_in_the_same_turn() {
    run(async {
        let (mut a, _rx) = agent(vec![
            Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("Still keep it or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ]);
        a.turn(Some("go".to_string().into())).await;
        assert_eq!(a.messages.len(), 4);
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no third request");
    });
}

#[test]
fn ask_nudge_is_skipped_after_an_ask_call() {
    run(async {
        let (mut a, rx) = agent(vec![
            Ok(assistant(vec![call("t1", "ask")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ]);
        // Nobody listens for the overlay: the ask call ends at once.
        drop(rx);
        a.turn(Some("go".to_string().into())).await;
        assert_eq!(a.messages.len(), 4, "user, ask call, its result, the question: no reminder");
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no third request");
    });
}

#[test]
fn ask_nudge_is_off_without_the_ask_tool() {
    run(async {
        let (n, left) = question_turn(RunConfig { excluded: vec!["ask"], ..Default::default() }).await;
        assert_eq!((n, left), (2, 1), "-p, ACP and custom agents exclude ask");
        let (n, left) = question_turn(RunConfig { subagent: true, ..Default::default() }).await;
        assert_eq!((n, left), (2, 1), "subagents");
        let (n, left) = question_turn(RunConfig { allow: Some(vec!["read".into()]), ..Default::default() }).await;
        assert_eq!((n, left), (2, 1), "an allow list without ask");
    });
}

#[test]
fn ask_nudge_follows_the_config_switch() {
    run(async {
        let mut cfg = Configs::default();
        cfg.mu.ask.nudge = false;
        let (mut a, _rx) = agent_with(
            vec![
                Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
                Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
            ],
            cfg,
        );
        a.turn(Some("go".to_string().into())).await;
        assert_eq!(a.messages.len(), 2);
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1);
    });
}

#[test]
fn each_user_turn_gets_its_own_nudge() {
    run(async {
        let (mut a, _rx) = agent(vec![
            Ok(assistant(vec![Block::text("Keep it or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text(".")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("Keep it or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text(".")], StopReason::Stop)),
        ]);
        a.turn(Some("one".to_string().into())).await;
        a.turn(Some("two".to_string().into())).await;
        assert_eq!(a.messages.len(), 8);
        assert!(a.ports.providers.scripted.borrow().is_empty());
    });
}

#[test]
fn the_nudge_is_not_echoed_on_resume() {
    run(async {
        let (mut a, mut rx) = agent(vec![]);
        a.messages.push(Message::user(mu_base::ask_nudge::REMINDER), None, true);
        a.replay_history();
        assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::UserPrompt(_))));
    });
}

#[test]
fn a_dismissal_is_kept_in_history_but_not_streamed() {
    run(async {
        let (mut a, mut rx) = agent(vec![
            Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text(".")], StopReason::Stop)),
        ]);
        a.turn(Some("go".to_string().into())).await;
        assert_eq!(a.messages.len(), 4);
        assert!(
            matches!(&a.messages[3], Message::Assistant { content, .. } if text_of(content) == "."),
            "kept in history"
        );
        let events = drain(&mut rx);
        assert!(events.iter().all(|e| !matches!(e, AgentEvent::Text(t) if t == ".")), "not streamed");
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Text(t) if t == "Keep the cache or drop it?")));
    });
}

#[test]
fn a_dismissal_is_not_rendered_on_resume() {
    run(async {
        let (mut a, mut rx) = agent(vec![]);
        a.messages.push(Message::user("go"), None, false);
        a.messages.push(assistant(vec![Block::text("Keep it or drop it?")], StopReason::Stop), None, false);
        a.messages.push(Message::user(mu_base::ask_nudge::REMINDER), None, true);
        a.messages.push(assistant(vec![Block::text(".")], StopReason::Stop), None, false);
        a.replay_history();
        let events = drain(&mut rx);
        assert!(events.iter().all(|e| !matches!(e, AgentEvent::Text(t) if t == ".")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Text(t) if t == "Keep it or drop it?")));
    });
}

#[test]
fn a_stop_skips_plan_dispatch_and_follow_ups() {
    run(async {
        let (mut a, mut rx) = agent(vec![
            Ok(assistant(vec![Block::text("hi")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ]);
        a.plan.exec_plan = Some(ExecPlan {
            content: "plan".into(),
            title: "T".into(),
            uri: "local://t-plan.md".into(),
            decision: ModeAction::Execute { model: None },
            mode: "plan".into(),
            base: None,
            approved_at: 0,
            stored: None,
        });
        a.ports.inbox.borrow_mut().follow_up.push_back("later".to_string().into());
        a.ports.inbox.borrow_mut().stop = true;
        a.turn(Some("go".to_string().into())).await;
        assert!(a.plan.exec_plan.is_none(), "the pending plan is dropped, not dispatched");
        assert_eq!(a.ports.inbox.borrow().follow_up.len(), 1, "the follow-up stays queued");
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no second request");
        assert_eq!(a.messages.len(), 2);
        while let Ok(e) = rx.try_recv() {
            assert!(!matches!(e, AgentEvent::Notice(n) if n.contains("Plan approved")));
        }
    });
}

#[test]
fn a_wake_delivers_the_queued_message_and_empties_the_inbox() {
    run(async {
        let (mut a, mut rx) = notify_agent(vec![Ok(assistant(vec![Block::text("PONG")], StopReason::Stop))]);
        a.ensure_session().await;
        let id = a.session_id();
        enqueue(&id, "steward", "say PONG").unwrap();
        a.turn(None).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::Woken)));
        assert!(
            evs.iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("mu notify from steward: say PONG")))
        );
        assert!(session_text(&a).contains("Message from steward (via mu notify"));
        assert!(!inbox::has_pending(&id));
        assert!(a.ports.providers.scripted.borrow().is_empty());
    });
}

#[test]
fn a_message_during_a_turn_joins_that_turn() {
    run(async {
        let started_dir = mu_testutil::tmp();
        let started = started_dir.join("started");
        let release = started_dir.join("release");
        let command = format!("touch {}; while [ ! -f {} ]; do sleep 0.02; done", started.display(), release.display());
        let sleep = Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": command}) };
        let script = vec![
            Ok(assistant(vec![sleep], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("one")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = notify_agent(script);
        a.ensure_session().await;
        let id = a.session_id();
        tokio::task::spawn_local(async move {
            mu_testutil::wait_until("the tool to start", || started.exists()).await;
            enqueue(&id, "steward", "also HELLO").unwrap();
            std::fs::write(&release, "").unwrap();
        });
        a.turn(Some("go".to_string().into())).await;
        assert!(a.ports.providers.scripted.borrow().is_empty(), "two requests in one turn");
        let evs = drain(&mut rx);
        assert_eq!(evs.iter().filter(|e| matches!(e, AgentEvent::TurnStart)).count(), 1);
        assert!(!evs.iter().any(|e| matches!(e, AgentEvent::Woken)));
        let at = |f: &dyn Fn(&Message) -> bool| a.messages.iter().position(f).unwrap();
        let result = at(&|m| matches!(m, Message::ToolResult { .. }));
        let notice = at(&|m| user_text(m).is_some_and(|t| t.contains("also HELLO")));
        let reply = at(&|m| matches!(m, Message::Assistant { content, .. } if text_of(content) == "one"));
        assert!(result < notice && notice < reply, "the notice goes out before the model's next reply");
        assert!(!inbox::has_pending(&a.session_id()));
    });
}

#[test]
fn a_print_run_does_not_fall_back_to_a_fresh_session_when_resume_fails() {
    mu_testutil::home();
    run(async {
        let (a, mut rx) = agent_run(
            vec![Ok(assistant(vec![Block::text("hi")], StopReason::Stop))],
            Configs::default(),
            RunConfig { exit_on_resume_failure: true, ..Default::default() },
        );
        let (tx, crx) = tokio::sync::mpsc::unbounded_channel();
        let missing = std::env::temp_dir().join(format!("mu-missing-{}.jsonl", uuid::Uuid::new_v4()));
        tx.send(Command::Resume(missing)).unwrap();
        tx.send(Command::EnsureSession).unwrap();
        tx.send(Command::Prompt("go".to_string().into())).unwrap();
        tx.send(Command::Exit).unwrap();
        let providers = a.ports.providers.clone();
        a.run(crx).await;
        assert_eq!(providers.scripted.borrow().len(), 1, "the prompt never ran");
        let (mut errored, mut exited, mut sessions) = (false, false, false);
        while let Ok(e) = rx.try_recv() {
            match e {
                AgentEvent::Error(_) => errored = true,
                AgentEvent::Exit => exited = true,
                AgentEvent::Status(s) if s.session_path.is_some() => sessions = true,
                _ => {}
            }
        }
        assert!(errored && exited && !sessions);
    });
}

#[test]
fn parking_a_plan_saves_it_and_leaves_plan_mode_keeping_the_model() {
    run(async {
        let cfg = Configs::from_models_toml("[sets.main]\ndefault = \"a/b\"\nplan = \"a/c:high\"\n");
        let args = json!({"slug": "t", "title": "T"});
        let call = Block::ToolCall { id: "p1".into(), name: "propose_plan".into(), arguments: args };
        let (mut a, mut rx) = agent_with(
            vec![
                Ok(assistant(vec![call], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("parked")], StopReason::Stop)),
            ],
            cfg,
        );
        a.cwd = tmp_dir();
        a.enter_mode("plan", false).await;
        assert_eq!((a.model.model.id.as_str(), a.model.thinking), ("a/c", Thinking::High), "plan mode uses the role");
        let r = a.tools.resolve("local://t-plan.md").unwrap();
        std::fs::create_dir_all(r.path.parent().unwrap()).unwrap();
        std::fs::write(&r.path, "do it\n").unwrap();
        let answer = async {
            let mut parked = None;
            while let Some(e) = rx.recv().await {
                match e {
                    AgentEvent::Review { reply, request } => {
                        assert!(request.cancel_turn, "a live review is not a stored one");
                        let _ = reply.send(choice("park"));
                    }
                    AgentEvent::PlanParked { id, .. } => parked = Some(id),
                    AgentEvent::TurnEnd { .. } => break,
                    _ => {}
                }
            }
            parked
        };
        let (_, parked) = futures_util::future::join(a.turn(Some("go".to_string().into())), answer).await;
        assert_eq!(parked.as_deref(), Some("t"));
        assert!(a.plan.mode.is_none());
        assert_eq!(
            (a.model.model.id.as_str(), a.model.thinking),
            ("a/c", Thinking::High),
            "parking keeps the model it planned with"
        );
        let store = plans::store_for(&a.cwd).await;
        let (meta, text) = store.load("t").unwrap();
        assert_eq!((meta.status, text.as_str()), (mu_base::events::Status::Pending, "do it\n"));
    });
}

fn enter_call(id: &str) -> Block {
    Block::ToolCall {
        id: id.into(),
        name: "enter_mode".into(),
        arguments: json!({"mode": "plan", "reason": "a big change"}),
    }
}

fn choice(id: &str) -> mu_base::events::ActionChoice {
    mu_base::events::ActionChoice { id: id.into(), text: None, model: None }
}

fn propose_call(id: &str, slug: &str) -> Block {
    Block::ToolCall { id: id.into(), name: "propose_plan".into(), arguments: json!({"slug": slug, "title": slug}) }
}

fn allow_once() -> ApprovalDecision {
    ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None, patterns: None, model: None }
}

/// Answers every approval with `answer()` and parks every plan review; returns the approval
/// requests and the ids of the parked plans.
async fn answer_plans(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    answer: impl Fn() -> ApprovalDecision,
) -> (Vec<mu_base::permissions::ApprovalRequest>, Vec<String>) {
    let (mut asked, mut parked) = (Vec::new(), Vec::new());
    while let Some(e) = rx.recv().await {
        match e {
            AgentEvent::Approve { request, reply } => {
                asked.push(request);
                let _ = reply.send(answer());
            }
            AgentEvent::Review { reply, .. } => {
                let _ = reply.send(choice("park"));
            }
            AgentEvent::PlanParked { id, .. } => parked.push(id),
            AgentEvent::TurnEnd { .. } => break,
            _ => {}
        }
    }
    (asked, parked)
}

fn write_plan(a: &Agent, slug: &str) {
    let r = a.tools.resolve(&format!("local://{slug}-plan.md")).unwrap();
    std::fs::create_dir_all(r.path.parent().unwrap()).unwrap();
    std::fs::write(&r.path, "do it\n").unwrap();
}

fn tool_names(a: &Agent) -> Vec<String> {
    a.tool_defs().into_iter().map(|d| d.name).collect()
}

#[test]
fn an_approved_enter_mode_turns_the_mode_on_for_the_rest_of_the_turn() {
    run(async {
        let cfg = Configs::from_models_toml("[sets.main]\ndefault = \"a/b\"\nplan = \"a/c:high\"\n");
        let script = vec![
            Ok(assistant(vec![enter_call("e1")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("planning")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent_with(script, cfg);
        a.cwd = tmp_dir();
        a.title.requested = true; // the title request would take a scripted reply
        assert!(tool_names(&a).contains(&"enter_mode".to_string()));
        assert!(!tool_names(&a).contains(&"propose_plan".to_string()));
        let turn = a.turn(Some("go".to_string().into()));
        let ((), (asked, _)) = futures_util::future::join(turn, answer_plans(rx, allow_once)).await;
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].title, "enter_mode: switch to plan mode");
        assert_eq!(asked[0].body, "a big change");
        assert!(asked[0].grant.is_none());
        assert!(a.plan.mode.as_ref().is_some_and(|p| p.by_model && p.name == "plan"));
        assert_eq!((a.model.model.id.as_str(), a.model.thinking), ("a/c", Thinking::High));
        let names = tool_names(&a);
        assert!(names.contains(&"propose_plan".to_string()) && !names.contains(&"enter_mode".to_string()));
        assert!(a.ports.providers.scripted.borrow().is_empty(), "the turn went on to a second request");
    });
}

#[test]
fn enter_mode_offers_the_cycle_roles_and_the_one_picked_replaces_the_modes_model() {
    run(async {
        let cfg = Configs::from_models_toml("[sets.main]\ndefault = \"a/b\"\nplan = \"a/c:high\"\nslow = \"a/d\"\n");
        let script = vec![
            Ok(assistant(vec![enter_call("e1")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("planning")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent_with(script, cfg);
        a.cwd = tmp_dir();
        a.title.requested = true;
        let current = a.model.model.id.clone();
        let pick_slow = || ApprovalDecision::Allow {
            scope: ApprovalScope::Once,
            note: None,
            patterns: None,
            model: Some("@slow".into()),
        };
        let turn = a.turn(Some("go".to_string().into()));
        let ((), (asked, _)) = futures_util::future::join(turn, answer_plans(rx, pick_slow)).await;
        let offered: Vec<&str> = asked[0].models.iter().map(|m| m.selector.as_str()).collect();
        assert!(offered.contains(&"@slow") && offered.contains(&current.as_str()), "{offered:?}");
        assert!(a.plan.mode.as_ref().is_some_and(|p| p.name == "plan"));
        assert_eq!(a.model.model.id, "a/d", "the picked role, not @plan");
        assert!(a.plan.entry_model.is_none(), "the pick is used once");
    });
}

#[test]
fn a_denied_enter_mode_leaves_the_mode_off_and_returns_the_note() {
    run(async {
        let script = vec![
            Ok(assistant(vec![enter_call("e1")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent(script);
        let deny = || ApprovalDecision::Deny { note: Some("just do it".into()) };
        futures_util::future::join(a.turn(Some("go".to_string().into())), answer_plans(rx, deny)).await;
        assert!(a.plan.mode.is_none());
        let r = results(&a);
        assert!(r[0].0 && r[0].1.contains("just do it"), "{r:?}");
        assert!(a.ports.providers.scripted.borrow().is_empty(), "the model answered the denial");
    });
}

#[test]
fn enter_mode_asks_even_when_every_call_is_allowed() {
    run(async {
        let script = vec![
            Ok(assistant(vec![enter_call("e1")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let run_cfg =
            RunConfig { permission_mode: Some(mu_base::permissions::PermissionMode::Yolo), ..Default::default() };
        let (mut a, rx) = agent_run(script, Configs::default(), run_cfg);
        a.cwd = tmp_dir();
        let (_, (asked, _)) =
            futures_util::future::join(a.turn(Some("go".to_string().into())), answer_plans(rx, allow_once)).await;
        assert_eq!(asked.len(), 1, "yolo mode still asks");
    });
}

#[test]
fn a_model_can_plan_twice_in_one_turn_and_continues_after_each_park() {
    run(async {
        let cfg = Configs::from_models_toml("[sets.main]\ndefault = \"a/b\"\nplan = \"a/c:high\"\n");
        let script = vec![
            Ok(assistant(vec![enter_call("e1")], StopReason::ToolUse)),
            Ok(assistant(vec![propose_call("p1", "one")], StopReason::ToolUse)),
            Ok(assistant(vec![enter_call("e2")], StopReason::ToolUse)),
            Ok(assistant(vec![propose_call("p2", "two")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("both parked")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent_with(script, cfg);
        a.cwd = tmp_dir();
        a.title.requested = true; // the title request would take a scripted reply
        a.ensure_session().await;
        write_plan(&a, "one");
        write_plan(&a, "two");
        let turn = a.turn(Some("write plans for one and two".to_string().into()));
        let (_, (asked, parked)) = futures_util::future::join(turn, answer_plans(rx, allow_once)).await;
        assert_eq!(asked.len(), 2, "each entry asks again");
        assert_eq!(parked, ["one", "two"]);
        assert!(a.plan.mode.is_none());
        assert_eq!((a.model.model.id.as_str(), a.model.thinking), ("a/c", Thinking::High));
        let store = plans::store_for(&a.cwd).await;
        assert!(store.load("one").is_ok() && store.load("two").is_ok());
        let park_results: Vec<String> =
            results(&a).into_iter().filter(|(_, t)| t.contains("Plan parked as")).map(|(_, t)| t).collect();
        assert_eq!(park_results.len(), 2);
        assert!(park_results.iter().all(|t| t.contains("Continue with what the user asked")), "{park_results:?}");
        assert!(a.ports.providers.scripted.borrow().is_empty());
    });
}

#[test]
fn a_park_after_the_users_own_plan_mode_still_ends_the_turn() {
    run(async {
        let script = vec![
            Ok(assistant(vec![propose_call("p1", "t")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("parked")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent(script);
        a.cwd = tmp_dir();
        a.enter_mode("plan", false).await;
        assert!(a.plan.mode.as_ref().is_some_and(|p| !p.by_model));
        write_plan(&a, "t");
        let turn = a.turn(Some("go".to_string().into()));
        let (_, (_, parked)) = futures_util::future::join(turn, answer_plans(rx, allow_once)).await;
        assert_eq!(parked, ["t"]);
        let r = results(&a);
        assert!(r[0].1.contains("End your turn now") && !r[0].1.contains("Continue with"), "{r:?}");
    });
}

#[test]
fn a_session_saved_in_a_model_entered_plan_mode_resumes_with_by_model_set() {
    run(async {
        let (mut a, _rx) = agent(vec![]);
        a.enter_mode("plan", true).await;
        let path = a.session.as_ref().unwrap().path.clone();
        a.end_session().await;
        a.reset_conversation();
        assert!(a.plan.mode.is_none());
        assert!(a.load_session(&path).await);
        assert!(a.plan.mode.as_ref().is_some_and(|p| p.by_model));

        a.exit_mode();
        a.enter_mode("plan", false).await;
        let path = a.session.as_ref().unwrap().path.clone();
        a.end_session().await;
        a.reset_conversation();
        assert!(a.load_session(&path).await);
        assert!(a.plan.mode.as_ref().is_some_and(|p| !p.by_model));
    });
}

#[test]
fn a_subagent_is_not_offered_enter_mode() {
    run(async {
        let run_cfg = RunConfig { subagent: true, excluded: HEADLESS_EXCLUDED.to_vec(), ..Default::default() };
        let (a, _rx) = agent_run(vec![], Configs::default(), run_cfg);
        let names = tool_names(&a);
        assert!(!names.contains(&"enter_mode".to_string()) && !names.contains(&"propose_plan".to_string()));
    });
}

/// A copy of the built-in plan mode named `name`, writing `local://<slug>-<name>.md`.
fn custom_mode(name: &str) -> mu_tools::context::ModeDef {
    use mu_base::events::ModeEffect::{Exit, Keep, Park, Refine};
    let mut m = mu_tools::context::builtin_modes().remove(0);
    m.name = name.into();
    m.exit.document = name.into();
    m.exit.actions =
        [(Keep, "Approve and keep context"), (Park, "Park {n}"), (Refine, "Refine {n}"), (Exit, "Leave {name}")]
            .map(|(e, l)| (e, l.to_string()))
            .to_vec();
    m.model = vec![];
    m
}

fn add_mode(a: &mut Agent, m: mu_tools::context::ModeDef) {
    Rc::get_mut(&mut a.ctx).expect("sole owner").modes.push(m);
}

fn enter_named(id: &str, mode: &str) -> Block {
    Block::ToolCall { id: id.into(), name: "enter_mode".into(), arguments: json!({"mode": mode, "reason": "r"}) }
}

fn write_doc(a: &Agent, slug: &str, doc: &str) {
    let r = a.tools.resolve(&format!("local://{slug}-{doc}.md")).unwrap();
    std::fs::create_dir_all(r.path.parent().unwrap()).unwrap();
    std::fs::write(&r.path, "the document\n").unwrap();
}

#[test]
fn a_mode_that_allows_entry_is_entered_without_asking() {
    run(async {
        let script = vec![
            Ok(assistant(vec![enter_named("e1", "free")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, rx) = agent(script);
        let mut m = custom_mode("free");
        m.model_entry = mu_tools::context::ModelEntry::Allow;
        add_mode(&mut a, m);
        a.cwd = tmp_dir();
        a.title.requested = true;
        let turn = a.turn(Some("go".to_string().into()));
        let (_, (asked, _)) = futures_util::future::join(turn, answer_plans(rx, allow_once)).await;
        assert!(asked.is_empty(), "no approval request: {asked:?}");
        assert!(a.plan.mode.as_ref().is_some_and(|m| m.name == "free" && m.by_model));
    });
}

#[test]
fn a_read_only_mode_refuses_the_tree_in_its_own_words() {
    run(async {
        let (mut a, _rx) = agent(vec![]);
        add_mode(&mut a, custom_mode("design"));
        a.cwd = tmp_dir();
        a.enter_mode("design", false).await;
        let args = json!({"path": "src/x", "content": "x"});
        let out = a.exec_tool("w1", "write", &args, &CancellationToken::new(), None).await;
        assert!(out.is_error);
        assert_eq!(
            out.text,
            "Design mode: the working tree is read-only. Write your design to a local://<slug>-design.md file instead."
        );
    });
}

#[test]
fn the_exit_action_leaves_the_mode_and_restores_the_model() {
    run(async {
        let cfg = Configs::from_models_toml("[sets.main]\ndefault = \"a/b\"\nplan = \"a/c:high\"\n");
        let script = vec![
            Ok(assistant(vec![propose_call("p1", "t")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = agent_with(script, cfg);
        let mut m = custom_mode("design");
        m.model = vec!["@plan".into()];
        add_mode(&mut a, m);
        a.cwd = tmp_dir();
        a.title.requested = true;
        let before = a.model.model.id.clone();
        a.enter_mode("design", false).await;
        assert_eq!(a.model.model.id, "a/c");
        write_doc(&a, "t", "design");
        let answer = async {
            while let Some(e) = rx.recv().await {
                match e {
                    AgentEvent::Review { reply, .. } => {
                        let _ = reply.send(choice("exit"));
                    }
                    AgentEvent::TurnEnd { .. } => break,
                    _ => {}
                }
            }
        };
        futures_util::future::join(a.turn(Some("go".to_string().into())), answer).await;
        assert!(a.plan.mode.is_none());
        assert_eq!(a.model.model.id, before, "leaving restores the model");
        let r = results(&a);
        assert!(r[0].1.contains("Design accepted; design mode is off."), "{r:?}");
        let store = plans::store_for(&a.cwd).await;
        assert!(store.list(true).0.is_empty(), "nothing is stored for an accepted document");
    });
}

#[test]
fn parking_in_a_custom_mode_records_it_and_the_stored_dialog_offers_a_drop() {
    run(async {
        let script = vec![
            Ok(assistant(vec![propose_call("p1", "t")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("parked")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = agent(script);
        add_mode(&mut a, custom_mode("design"));
        a.cwd = tmp_dir();
        a.title.requested = true;
        a.enter_mode("design", false).await;
        write_doc(&a, "t", "design");
        let answer = async {
            while let Some(e) = rx.recv().await {
                match e {
                    AgentEvent::Review { reply, .. } => {
                        let _ = reply.send(choice("park"));
                    }
                    AgentEvent::TurnEnd { .. } => break,
                    _ => {}
                }
            }
        };
        futures_util::future::join(a.turn(Some("go".to_string().into())), answer).await;
        let store = plans::store_for(&a.cwd).await;
        let (meta, _) = store.load("t").unwrap();
        assert_eq!(meta.mode, "design");
        let respond = async {
            while let Some(e) = rx.recv().await {
                if let AgentEvent::Review { request, reply } = e {
                    let _ = reply.send(choice("drop"));
                    return request;
                }
            }
            panic!("no dialog")
        };
        let (start, request) = futures_util::future::join(a.open_plan("t"), respond).await;
        assert!(start.is_none());
        assert_eq!(request.header, "Design review");
        assert!(request.actions.iter().any(|x| x.label == "Drop design") && !request.cancel_turn);
        assert_eq!(store.load("t").unwrap().0.status, mu_base::events::Status::Dropped);
    });
}

#[test]
fn resuming_in_a_mode_that_is_not_defined_here_says_so() {
    run(async {
        let (mut a, mut rx) = agent(vec![]);
        a.ensure_session().await;
        a.session.as_mut().unwrap().mode_change("ghost", json!({}));
        let path = a.session.as_ref().unwrap().path.clone();
        a.end_session().await;
        a.reset_conversation();
        drain(&mut rx);
        assert!(a.load_session(&path).await);
        assert!(a.plan.mode.is_none());
        let notes: Vec<String> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| if let AgentEvent::Notice(n) = e { Some(n) } else { None })
            .collect();
        assert!(notes.iter().any(|n| n == "mode ghost is not defined here; resumed without it"), "{notes:?}");
    });
}

#[test]
fn exit_tools_are_mode_bound_and_a_mode_can_deny_tools() {
    run(async {
        let script = vec![
            Ok(assistant(vec![propose_call("p1", "t")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
            Ok(assistant(vec![call("b1", "bash")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, _rx) = agent(script);
        let mut m = custom_mode("quiet");
        m.deny = vec!["bash".into()];
        add_mode(&mut a, m);
        a.title.requested = true;
        a.turn(Some("go".to_string().into())).await;
        a.enter_mode("quiet", false).await;
        a.turn(Some("again".to_string().into())).await;
        let r = results(&a);
        assert!(r[0].0 && r[0].1.contains("only available in plan mode"), "{r:?}");
        assert!(r[1].0 && r[1].1.contains("not available in quiet mode"), "{r:?}");
    });
}

#[test]
fn a_stored_plan_starts_in_the_current_session() {
    run(async {
        let (mut a, _rx) = agent(vec![Ok(assistant(vec![Block::text("done")], StopReason::Stop))]);
        a.cwd = git_repo();
        let store = save_plan_in(&a.cwd, &["a.txt"]).await;
        a.turn_from(Start::Plan(ExecPlan {
            content: "do it\n".into(),
            title: "T".into(),
            uri: "local://t-plan.md".into(),
            decision: ModeAction::Keep { model: None },
            mode: "plan".into(),
            base: mu_base::git::snapshot_tree(&a.cwd).await.ok(),
            approved_at: 1,
            stored: Some("t".into()),
        }))
        .await;
        assert!(a.ports.providers.scripted.borrow().is_empty(), "exactly one request was made");
        let plan_msg = a.messages.iter().find_map(|m| match m {
            Message::User { content, synthetic: true, .. } => Some(text_of(content)),
            _ => None,
        });
        assert!(plan_msg.unwrap().contains("<plan path=\"local://t-plan.md\">"));
        let (meta, _) = store.load("t").unwrap();
        assert_eq!(meta.status, mu_base::events::Status::Approved);
        let session = a.session.as_ref().map(|s| s.path.clone());
        assert!(session.is_some() && meta.approved.unwrap().session == session);
        assert_eq!(a.plan.review_base.unwrap().plan_uri.as_deref(), Some("local://t-plan.md"));
    });
}

#[test]
fn a_failed_reopen_of_the_current_session_keeps_it_and_its_conversation() {
    use std::os::unix::fs::PermissionsExt;
    run(async {
        let (mut a, mut rx) = agent(vec![Ok(assistant(vec![Block::text("ok")], StopReason::Stop))]);
        a.ensure_session().await;
        a.turn(Some("hi".to_string().into())).await;
        let path = a.session.as_ref().unwrap().path.clone();
        let before = a.messages.len();
        assert!(before > 0);
        // Loads fine, but cannot be opened for appending.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        if std::fs::OpenOptions::new().append(true).open(&path).is_ok() {
            return; // running as root (the release container): permissions do not apply
        }
        drain(&mut rx);
        let ok = a.load_session(&path).await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!ok);
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Error(_))));
        assert_eq!(a.messages.len(), before);
        assert_eq!(a.session.as_ref().map(|s| s.path.clone()), Some(path));
    });
}

#[test]
fn messages_waiting_at_open_are_announced_and_go_out_with_the_next_prompt() {
    run(async {
        let (mut a, mut rx) = notify_agent(vec![Ok(assistant(vec![Block::text("ok")], StopReason::Stop))]);
        a.ensure_session().await;
        let (id, path) = (a.session_id(), a.session.as_ref().unwrap().path.clone());
        a.end_session().await;
        a.reset_conversation();
        enqueue(&id, "steward", "later").unwrap();
        drain(&mut rx);
        a.load_session(&path).await;
        let evs = drain(&mut rx);
        assert!(
            evs.iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("1 message(s) from mu notify waiting")))
        );
        assert!(!evs.iter().any(|e| matches!(e, AgentEvent::TurnStart)));
        assert!(inbox::has_pending(&id));
        a.turn(None).await;
        assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::TurnStart)), "announced is not a wake");
        a.turn(Some("hi".to_string().into())).await;
        let users = user_texts(&a);
        assert_eq!(users.len(), 2, "{users:?}");
        assert!(users[0].contains("Message from steward") && users[1] == "hi", "{users:?}");
        assert!(!inbox::has_pending(&id));
    });
}

#[test]
fn the_waiting_notice_follows_the_replayed_history() {
    run(async {
        let (mut a, mut rx) = notify_agent(vec![Ok(assistant(vec![Block::text("earlier reply")], StopReason::Stop))]);
        a.ensure_session().await;
        a.turn(Some("earlier prompt".to_string().into())).await;
        let (id, path) = (a.session_id(), a.session.as_ref().unwrap().path.clone());
        a.end_session().await;
        a.reset_conversation();
        enqueue(&id, "steward", "later").unwrap();
        drain(&mut rx);
        a.load_session(&path).await;
        let evs = drain(&mut rx);
        let pos = |f: &dyn Fn(&AgentEvent) -> bool| evs.iter().position(f);
        let waiting: Vec<usize> = evs
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, AgentEvent::Notice(n) if n.contains("1 message(s) from mu notify waiting")))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(waiting.len(), 1, "notice emitted exactly once");
        let prompt =
            pos(&|e| matches!(e, AgentEvent::UserPrompt(t) if t == "earlier prompt")).expect("prompt replayed");
        let reply = pos(&|e| matches!(e, AgentEvent::Text(t) if t == "earlier reply")).expect("reply replayed");
        assert!(prompt < waiting[0] && reply < waiting[0], "notice follows the replayed history");
        assert!(inbox::has_pending(&id), "still delivered with the next prompt, not at open");
    });
}

#[test]
fn rewind_takes_a_prompt_back_and_the_session_continues_from_before_it() {
    run(async {
        let reply = |t: &str| Ok(assistant(vec![Block::text(t)], StopReason::Stop));
        let (mut a, mut rx) = agent(vec![reply("r1"), reply("r2"), reply("r3")]);
        a.ensure_session().await;
        a.turn(Some("first".to_string().into())).await;
        a.turn(Some("second".to_string().into())).await;
        drain(&mut rx);

        a.list_rewind();
        let points = drain(&mut rx)
            .into_iter()
            .find_map(|e| if let AgentEvent::RewindPoints(p) = e { Some(p) } else { None })
            .expect("the points are listed");
        assert_eq!(points.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["first", "r1", "second"]);
        assert_eq!(
            points.iter().map(|p| p.reply).collect::<Vec<_>>(),
            [false, true, false],
            "the last reply is not offered"
        );

        let path = a.session.as_ref().unwrap().path.clone();
        a.rewind(&points[2].id).await;
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::QueuePopped(p) if p.text == "second")));
        assert!(!evs.iter().any(|e| matches!(e, AgentEvent::UserPrompt(_))), "the screen is not replayed");
        assert_eq!(user_texts(&a), ["first"]);
        assert_eq!(a.messages.len(), 2);
        assert_eq!(a.session.as_ref().unwrap().path, path, "same file");

        a.turn(Some("second, reworded".to_string().into())).await;
        assert_eq!(user_texts(&a), ["first", "second, reworded"]);
        let loaded = mu_session::session::format::load(&path).unwrap();
        assert_eq!(loaded.messages.len(), 4, "the abandoned branch is not loaded");
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"second\""), "but it is kept in the file");
    });
}

#[test]
fn rewind_to_a_reply_keeps_it_and_drops_what_came_after() {
    run(async {
        let reply = |t: &str| Ok(assistant(vec![Block::text(t)], StopReason::Stop));
        let (mut a, mut rx) = agent(vec![reply("r1"), reply("r2"), reply("r3")]);
        a.ensure_session().await;
        a.turn(Some("first".to_string().into())).await;
        a.turn(Some("second".to_string().into())).await;
        drain(&mut rx);

        a.list_rewind();
        let points = drain(&mut rx)
            .into_iter()
            .find_map(|e| if let AgentEvent::RewindPoints(p) = e { Some(p) } else { None })
            .expect("the points are listed");
        let path = a.session.as_ref().unwrap().path.clone();
        a.rewind(&points[1].id).await;
        let evs = drain(&mut rx);
        assert!(!evs.iter().any(|e| matches!(e, AgentEvent::QueuePopped(_))), "nothing goes back to the composer");
        assert_eq!(user_texts(&a), ["first"]);
        assert_eq!(a.messages.len(), 2, "the reply stays");

        a.turn(Some("third".to_string().into())).await;
        assert_eq!(user_texts(&a), ["first", "third"]);
        let loaded = mu_session::session::format::load(&path).unwrap();
        assert_eq!(loaded.messages.len(), 4, "the abandoned branch is not loaded");
    });
}

#[test]
fn rewind_offers_only_what_the_user_typed_and_the_agent_replied() {
    run(async {
        let (mut a, mut rx) = agent(vec![Ok(assistant(vec![Block::text("ok")], StopReason::Stop))]);
        a.ensure_session().await;
        a.turn(Some("typed".to_string().into())).await;
        a.messages.push(Message::synthetic_user("plan"), Some("p".into()), false);
        a.messages.push(Message::user("<system-reminder>\nnote\n</system-reminder>"), Some("r".into()), false);
        drain(&mut rx);
        a.list_rewind();
        let points = drain(&mut rx)
            .into_iter()
            .find_map(|e| if let AgentEvent::RewindPoints(p) = e { Some(p) } else { None })
            .unwrap();
        assert_eq!(
            points.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["typed", "ok"],
            "not mu's own messages"
        );
        a.rewind("nope").await;
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Error(_))));
    });
}

#[test]
fn fork_continues_in_a_copy_and_leaves_the_original_alone() {
    run(async {
        let reply = |t: &str| Ok(assistant(vec![Block::text(t)], StopReason::Stop));
        let (mut a, mut rx) = agent(vec![reply("r1"), reply("r2")]);
        a.fork().await;
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::Error(m) if m.contains("nothing to fork"))));

        a.ensure_session().await;
        a.turn(Some("first".to_string().into())).await;
        let original = a.session.as_ref().unwrap().path.clone();
        drain(&mut rx);
        a.fork().await;
        let forked = a.session.as_ref().unwrap().path.clone();
        assert_ne!(forked, original);
        let evs = drain(&mut rx);
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("forked"))));
        assert!(!evs.iter().any(|e| matches!(e, AgentEvent::UserPrompt(_))), "the screen is not replayed");
        assert_eq!(user_texts(&a), ["first"]);

        a.turn(Some("only in the fork".to_string().into())).await;
        assert_eq!(mu_session::session::format::load(&forked).unwrap().messages.len(), 4);
        assert_eq!(mu_session::session::format::load(&original).unwrap().messages.len(), 2);
    });
}

#[test]
fn a_failed_compaction_is_not_retried_every_tool_round() {
    run(async {
        let script = vec![
            Ok(assistant(vec![Block::text("hello")], StopReason::Stop)),
            Err(ProviderError::Fatal("summariser down".into())),
            Ok(assistant(vec![call("t1", "read")], StopReason::ToolUse)),
            Ok(assistant(vec![call("t2", "read")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let cfg = Configs::from_models_toml("[compaction.thresholds]\n\"anthropic/m\" = \"f1000\"\n");
        let (mut a, mut rx) = agent_with(script, cfg);
        a.turn(Some("hi".to_string().into())).await;
        let big = "x".repeat(8_000);
        for _ in 0..3 {
            a.push(Message::user(&big));
        }
        drain(&mut rx);
        a.turn(Some("go".to_string().into())).await;
        let failures = drain(&mut rx)
            .into_iter()
            .filter(|e| matches!(e, AgentEvent::Error(m) if m.starts_with("compaction failed")))
            .count();
        assert_eq!(failures, 1, "one summariser request and one error line for the whole turn");
        assert!(a.compaction.failed_at.is_some());
        assert_eq!(results(&a).len(), 2, "the turn went on through both tool rounds");
    });
}

fn summary_reply(text: &str) -> Result<Message, ProviderError> {
    Ok(assistant(vec![Block::text(text)], StopReason::Stop))
}

/// A first turn, then three large messages (about 25k tokens each), so history is long enough to cut
/// while the default threshold is not yet reached.
async fn long_history(
    script: Vec<Result<Message, ProviderError>>,
    cfg: Configs,
) -> (Agent, UnboundedReceiver<AgentEvent>) {
    let mut s = vec![Ok(assistant(vec![Block::text("hello")], StopReason::Stop))];
    s.extend(script);
    let (mut a, mut rx) = agent_with(s, cfg);
    a.turn(Some("hi".to_string().into())).await;
    let big = "x".repeat(100_000);
    for _ in 0..3 {
        a.push(Message::user(&big));
    }
    drain(&mut rx);
    (a, rx)
}

/// The history and the session file agree: same messages, same entry ids, same summary.
fn assert_matches_session(a: &Agent) {
    let loaded = mu_session::session::format::load(&a.session.as_ref().unwrap().path).unwrap();
    assert_eq!(loaded.messages.len(), a.messages.len());
    let ids: Vec<String> = a.messages.entries().iter().map(|e| e.id.clone().unwrap_or_default()).collect();
    assert_eq!(loaded.entry_ids, ids);
    assert_eq!(loaded.compaction.map(|c| c.summary), a.compaction.state.as_ref().map(|c| c.summary.clone()));
}

fn errors(events: &[AgentEvent]) -> Vec<&str> {
    events.iter().filter_map(|e| if let AgentEvent::Error(m) = e { Some(m.as_str()) } else { None }).collect()
}

#[test]
fn a_prompt_over_the_threshold_is_compacted_before_it_is_sent() {
    run(async {
        let cfg = Configs::from_models_toml("[compaction.thresholds]\n\"anthropic/m\" = \"f1000\"\n");
        let script = vec![summary_reply("SUMMARY"), Ok(assistant(vec![Block::text("done")], StopReason::Stop))];
        let (mut a, mut rx) = long_history(script, cfg).await;
        a.turn(Some("go".to_string().into())).await;
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Compacting)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Compacted { .. })));
        assert!(errors(&events).is_empty() && !events.iter().any(|e| matches!(e, AgentEvent::TurnFailed)));
        assert_eq!(a.compaction.state.as_ref().map(|c| c.summary.as_str()), Some("SUMMARY"));
        assert_eq!(user_texts(&a).last().map(String::as_str), Some("go"), "the prompt follows the kept tail");
        assert_eq!(a.messages.len(), 3, "one kept message, the prompt and the reply");
        assert!(a.ports.providers.scripted.borrow().is_empty());
        assert_matches_session(&a);
    });
}

#[test]
fn a_tool_round_over_the_threshold_compacts_again_before_the_next_request() {
    run(async {
        let cfg = Configs::from_models_toml("[compaction.thresholds]\n\"anthropic/m\" = \"f1000\"\n");
        let script = vec![
            summary_reply("FIRST"),
            Ok(assistant(vec![call("t1", "__panic")], StopReason::ToolUse)),
            summary_reply("SECOND"),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = long_history(script, cfg).await;
        a.turn(Some("go".to_string().into())).await;
        let events = drain(&mut rx);
        let compacted = events.iter().filter(|e| matches!(e, AgentEvent::Compacted { .. })).count();
        assert_eq!(compacted, 2);
        assert_eq!(a.compaction.state.as_ref().map(|c| c.summary.as_str()), Some("SECOND"));
        assert!(matches!(a.messages.last(), Some(Message::Assistant { .. })));
        assert!(a.ports.providers.scripted.borrow().is_empty());
        assert_matches_session(&a);
    });
}

#[test]
fn overflow_compacts_once_and_the_request_is_retried() {
    run(async {
        let script = vec![
            Err(ProviderError::Overflow("prompt is too long".into())),
            summary_reply("SUMMARY"),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = long_history(script, Configs::default()).await;
        a.turn(Some("go".to_string().into())).await;
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Compacted { .. })));
        assert!(errors(&events).is_empty() && !events.iter().any(|e| matches!(e, AgentEvent::TurnFailed)));
        assert!(matches!(a.messages.last(), Some(Message::Assistant { .. })));
        assert_eq!(a.compaction.state.as_ref().map(|c| c.summary.as_str()), Some("SUMMARY"));
        assert_eq!(a.messages.len(), 2, "the prompt and the reply survive the cut");
        assert!(a.ports.providers.scripted.borrow().is_empty());
        assert_matches_session(&a);
    });
}

#[test]
fn a_second_overflow_after_compaction_fails_the_turn() {
    run(async {
        let script = vec![
            Err(ProviderError::Overflow("prompt is too long".into())),
            summary_reply("SUMMARY"),
            Err(ProviderError::Overflow("prompt is too long".into())),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = long_history(script, Configs::default()).await;
        a.turn(Some("go".to_string().into())).await;
        let events = drain(&mut rx);
        assert_eq!(errors(&events), ["context overflow after compaction"]);
        let failed = events.iter().position(|e| matches!(e, AgentEvent::TurnFailed)).expect("TurnFailed");
        let ended = events.iter().position(|e| matches!(e, AgentEvent::TurnEnd { .. })).expect("TurnEnd");
        assert!(failed < ended);
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no third request");
        assert_matches_session(&a);
    });
}

#[test]
fn overflow_with_nothing_to_compact_reports_the_provider_error() {
    run(async {
        let (mut a, mut rx) = agent(vec![Err(ProviderError::Overflow("prompt is too long".into()))]);
        a.turn(Some("go".to_string().into())).await;
        let events = drain(&mut rx);
        assert_eq!(errors(&events), ["error: prompt is too long"]);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnFailed)));
        assert!(a.compaction.state.is_none());
    });
}

#[test]
fn overflow_with_a_failing_summariser_fails_the_turn_once() {
    run(async {
        let script = vec![
            Err(ProviderError::Overflow("prompt is too long".into())),
            Err(ProviderError::Fatal("summariser down".into())),
        ];
        let (mut a, mut rx) = long_history(script, Configs::default()).await;
        a.turn(Some("go".to_string().into())).await;
        let events = drain(&mut rx);
        let failures = errors(&events).iter().filter(|m| m.starts_with("compaction failed")).count();
        assert_eq!(failures, 1);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnFailed)));
        assert!(a.compaction.state.is_none());
        assert_matches_session(&a);
    });
}

#[test]
fn the_real_loop_wakes_on_the_channel() {
    run(async {
        let (mut a, mut rx) = agent(vec![Ok(assistant(vec![Block::text("PONG")], StopReason::Stop))]);
        a.enable_jobs();
        let wake = a.enable_notify().unwrap();
        a.ensure_session().await;
        let (id, path) = (a.session_id(), a.session.as_ref().unwrap().path.clone());
        let (ctx, crx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::task::spawn_local(a.run(crx));
        enqueue(&id, "steward", "say PONG").unwrap();
        wake.send(()).unwrap();
        let waited = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while let Some(e) = rx.recv().await {
                if matches!(e, AgentEvent::TurnEnd { .. }) {
                    return;
                }
            }
        })
        .await;
        assert!(waited.is_ok(), "no turn ran");
        ctx.send(Command::Exit).unwrap();
        drop(ctx);
        let _ = task.await;
        assert!(std::fs::read_to_string(path).unwrap().contains("Message from steward (via mu notify"));
        assert!(!inbox::has_pending(&id));
    });
}

#[test]
fn a_drifted_plan_is_refused_without_anyway() {
    run(async {
        let (mut a, mut rx) = agent(vec![Ok(assistant(vec![Block::text("never")], StopReason::Stop))]);
        a.cwd = git_repo();
        save_plan_in(&a.cwd, &["a.txt"]).await;
        std::fs::write(a.cwd.join("a.txt"), "two\n").unwrap();
        let start = a.start_plan("t", ModeAction::Keep { model: None }, false).await;
        assert!(start.is_none());
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no request was made");
        let mut stale = false;
        while let Ok(e) = rx.try_recv() {
            stale |= matches!(e, AgentEvent::Error(m) if m.contains("is stale"));
        }
        assert!(stale);
        assert!(a.start_plan("t", ModeAction::Keep { model: None }, true).await.is_some());
    });
}

#[test]
fn two_senders_and_one_wake_give_one_notice() {
    run(async {
        let (mut a, _rx) = notify_agent(vec![Ok(assistant(vec![Block::text("ok")], StopReason::Stop))]);
        a.ensure_session().await;
        let id = a.session_id();
        enqueue(&id, "a", "first").unwrap();
        enqueue(&id, "b", "second").unwrap();
        a.turn(None).await;
        let users = user_texts(&a);
        assert_eq!(users.len(), 1, "{users:?}");
        assert!(users[0].contains("Message from a") && users[0].contains("Message from b"));
    });
}

#[test]
fn a_native_model_pauses_the_quota_and_shows_no_windows() {
    run(async {
        let (mut a, _rx) = agent(vec![]);
        a.costs.quota.borrow_mut().enabled = true;
        a.costs
            .quota
            .borrow_mut()
            .set_usage(mu_base::events::ProviderUsage { provider: "anthropic".into(), windows: vec![] });
        a.model.model = ModelSpec::gateway("anthropic/m", 200_000, 8192);
        a.model.model.native = true;
        a.sync_model_state();
        assert!(a.costs.quota.borrow().wait() >= std::time::Duration::from_secs(3600), "the run loop must not spin");
        assert!(a.snapshot().usage.is_none(), "subscription windows never show under a per-token model");
        assert!(!a.model.pricing_checked, "a native-only session never starts omp");
        a.model.model.native = false;
        a.sync_model_state();
        assert!(a.model.pricing_checked, "the first gateway model reads omp's catalogue, once");
    });
}

#[test]
fn a_keyless_custom_model_is_reported_and_stays_native_when_its_definition_vanishes() {
    use mu_provider::provider::custom::{AuthMode, CustomProvider};
    run(async {
        let (mut a, mut rx) = agent(vec![]);
        let reg = a.ports.providers.custom_registry().clone();
        let mut c = CustomProvider::new("http://127.0.0.1:9/v1");
        c.auth = AuthMode::None;
        reg.add("agent-cust", c).unwrap();
        a.set_model("agent-cust/org/m");
        let s = a.snapshot();
        assert!(s.native && s.native_keyless && !s.reasoning, "{s:?}");
        assert!(a.model.model.native && a.model.model.provider == "agent-cust");
        assert!(!a.model.pricing_checked, "a custom endpoint never starts omp");
        assert!(reg.remove("agent-cust").unwrap());
        a.refresh_model_meta();
        assert!(a.model.model.native, "the next request fails locally instead of reaching the gateway");
        assert!(!a.snapshot().native_keyless);
        drain(&mut rx);
    });
}

#[test]
fn without_notify_pending_files_are_ignored() {
    run(async {
        let (mut a, mut rx) = agent(vec![]);
        a.enable_jobs(); // interactive-like, but `enable_notify` was never called (-p, ACP)
        a.ensure_session().await;
        let (id, path) = (a.session_id(), a.session.as_ref().unwrap().path.clone());
        enqueue(&id, "steward", "x").unwrap();
        a.turn(None).await;
        assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::TurnStart)));
        a.end_session().await;
        a.load_session(&path).await;
        assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::Notice(n) if n.contains("waiting"))));
        assert!(inbox::has_pending(&id));
    });
}

/// omp's soft request budget: the notice at the budget, the stop at 1.5x, tool calls refused after
/// it, and the run ended five requests later, so a scout cannot survey forever.
#[test]
fn a_subagent_budget_steers_then_stops_the_run() {
    run(async {
        let reply = |i: usize| {
            Ok(assistant(
                vec![Block::ToolCall {
                    id: format!("t{i}"),
                    name: "read".into(),
                    arguments: json!({"path": "does-not-exist"}),
                }],
                StopReason::ToolUse,
            ))
        };
        let script = (0..12).map(reply).collect();
        let run_cfg = RunConfig { subagent: true, request_budget: 2, budget_notice: true, ..Default::default() };
        // Requests 1: nothing, 2: the notice, 3: the stop, 4-7: refused calls, 8: the end.
        let (mut a, _rx) = agent_run(script, Configs::default(), run_cfg);
        a.turn(Some("go".to_string().into())).await;
        let said: Vec<String> = a
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User { content, .. } => Some(text_of(content)),
                _ => None,
            })
            .collect();
        let notices: Vec<&String> = said.iter().filter(|t| t.contains("[budget notice]")).collect();
        assert_eq!(notices.len(), 2, "one wrap-up notice and one stop: {said:?}");
        assert!(notices[0].contains("soft budget: 2") && notices[0].contains("At 3 requests"), "{}", notices[0]);
        assert!(notices[1].contains("no more tools will run"), "{}", notices[1]);
        let skipped = a
            .messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResult { content, .. } if text_of(content).contains("past its request budget")))
            .count();
        assert!(skipped >= 1, "tool calls after the stop are refused: {skipped}");
        let replies = a.messages.iter().filter(|m| matches!(m, Message::Assistant { .. })).count();
        assert_eq!(replies, 7, "requests 1-7, then the 8th step ends the run");
    });
}

/// The stop refuses tools, but a `yield` in the stopped reply still lands: a structured subagent
/// delivers its result instead of failing as if it never submitted one.
#[test]
fn a_yield_after_the_budget_stop_still_lands() {
    run(async {
        let script = vec![
            Ok(assistant(vec![call("t1", "read")], StopReason::ToolUse)),
            Ok(assistant(vec![call("t2", "read")], StopReason::ToolUse)),
            Ok(assistant(vec![yield_call("y1", json!({"data":{"n":1}})), call("t3", "__panic")], StopReason::ToolUse)),
        ];
        let run_cfg = RunConfig { subagent: true, request_budget: 1, budget_notice: true, ..Default::default() };
        let (mut a, _rx) = agent_run(script, Configs::default(), run_cfg);
        *a.tools.output_schema.borrow_mut() =
            Some(json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}));
        a.turn(Some("go".to_string().into())).await;
        let r = results(&a);
        assert!(r.iter().any(|(err, text)| !err && text.contains("Result submitted")), "{r:?}");
        assert!(r.iter().any(|(err, text)| *err && text.contains("past its request budget")), "{r:?}");
        assert!(!r.iter().any(|(_, text)| text.contains("panicked")), "the tool after the yield never ran: {r:?}");
    });
}

fn fifo(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    let ok = std::process::Command::new("mkfifo").arg(&p).status().unwrap().success();
    assert!(ok, "mkfifo {}", p.display());
    p
}

/// A write to a FIFO returns once a reader has it open and takes the text.
async fn feed(path: &Path, text: &'static str) {
    let p = path.to_owned();
    tokio::task::spawn_blocking(move || std::fs::write(p, text)).await.unwrap().unwrap();
}

fn cat_call(id: &str, path: &Path) -> Block {
    bash_call(id, json!({"command": format!("cat {}", path.display())}))
}

fn call_events(events: Vec<AgentEvent>) -> Vec<String> {
    events
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::ToolStart { id, .. } => Some(format!("start {id}")),
            AgentEvent::ToolEnd { id, .. } => Some(format!("end {id}")),
            _ => None,
        })
        .collect()
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(30), f).await.expect("the turn hung")
}

#[test]
fn read_only_calls_run_together_and_results_keep_call_order() {
    run(async {
        let dir = tmp_dir();
        let (f1, f2) = (fifo(&dir, "f1"), fifo(&dir, "f2"));
        let script = vec![
            Ok(assistant(vec![cat_call("t1", &f1), cat_call("t2", &f2)], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = agent_in(script, Configs::default(), RunConfig::default(), dir);
        a.enable_jobs(); // foreground bash then runs as its own process; the headless shell runs one line at a time
        // Run one after the other, t1 would wait for its text forever: t2 must finish first.
        let feeder = async {
            feed(&f2, "two").await;
            feed(&f1, "one").await;
        };
        within(futures_util::future::join(a.turn(Some("go".to_string().into())), feeder)).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        let r = results(&a);
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r[0].1.contains("one") && r[1].1.contains("two"), "call order, not finish order: {r:?}");
        assert_eq!(call_events(drain(&mut rx)), ["start t1", "start t2", "end t1", "end t2"]);
    });
}

#[test]
fn a_write_between_reads_is_a_barrier() {
    run(async {
        let dir = tmp_dir();
        let (f1, f2) = (fifo(&dir, "f1"), fifo(&dir, "f2"));
        let marker = dir.join("marker");
        let write = bash_call("t2", json!({"command": format!("touch {}", marker.display())}));
        let script = vec![
            Ok(assistant(vec![cat_call("t1", &f1), write, cat_call("t3", &f2)], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = agent_in(script, Configs::default(), RunConfig::default(), dir);
        a.enable_jobs();
        let feeder = async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let early = marker.exists();
            feed(&f1, "one").await;
            // t3 reads f2 only once the write has run.
            mu_testutil::wait_until("the write", || marker.exists()).await;
            feed(&f2, "three").await;
            early
        };
        let ((), early) = within(futures_util::future::join(a.turn(Some("go".to_string().into())), feeder)).await;
        assert!(!early, "the write waited for the read before it");
        let r = results(&a);
        assert_eq!(r.len(), 3, "{r:?}");
        assert!(r[0].1.contains("one") && r[2].1.contains("three"), "{r:?}");
        assert_eq!(call_events(drain(&mut rx)), ["start t1", "end t1", "start t2", "end t2", "start t3", "end t3"]);
    });
}

#[test]
fn abort_cancels_every_call_in_flight() {
    run(async {
        let dir = tmp_dir();
        let (f1, f2) = (fifo(&dir, "f1"), fifo(&dir, "f2"));
        let script = vec![Ok(assistant(vec![cat_call("t1", &f1), cat_call("t2", &f2)], StopReason::ToolUse))];
        let (mut a, mut rx) = agent_in(script, Configs::default(), RunConfig::default(), dir);
        a.enable_jobs();
        let inbox = a.ports.inbox.clone();
        let feeder = async {
            // Both writers open only once both readers run; holding them keeps the readers waiting.
            let open =
                |p: PathBuf| tokio::task::spawn_blocking(move || std::fs::OpenOptions::new().write(true).open(p));
            let (w1, w2) = futures_util::future::join(open(f1.clone()), open(f2.clone())).await;
            inbox.borrow().cancel.cancel();
            (w1.unwrap().unwrap(), w2.unwrap().unwrap())
        };
        let ((), _held) = within(futures_util::future::join(a.turn(Some("go".to_string().into())), feeder)).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
        assert_eq!(results(&a).len(), 2);
        let events = drain(&mut rx);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnEnd { aborted: true })));
        assert_eq!(call_events(events), ["start t1", "start t2", "end t1", "end t2"]);
    });
}

/// A read-only call that needs approval waits for the calls before it, then asks alone.
async fn asking_read_after_a_running_read(answer: ApprovalDecision) -> (Vec<(bool, String)>, usize) {
    let dir = tmp_dir();
    let outside = tmp_dir().join("x.txt");
    std::fs::write(&outside, "outside").unwrap();
    let f1 = fifo(&dir, "f1");
    let script = vec![
        Ok(assistant(
            vec![cat_call("t1", &f1), bash_call("t2", json!({"command": format!("cat {}", outside.display())}))],
            StopReason::ToolUse,
        )),
        Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
    ];
    let run_cfg = RunConfig { permission_mode: Some(mu_base::permissions::PermissionMode::Ask), ..Default::default() };
    let (mut a, mut rx) = agent_in(script, Configs::default(), run_cfg, dir);
    a.enable_jobs();
    let asked = Rc::new(Cell::new(0usize));
    let asked_in = asked.clone();
    let answers = async move {
        while let Some(e) = rx.recv().await {
            match e {
                AgentEvent::Approve { reply, .. } => {
                    asked_in.set(asked_in.get() + 1);
                    let _ = reply.send(answer.clone());
                }
                AgentEvent::TurnEnd { .. } => break,
                _ => {}
            }
        }
    };
    let early = Rc::new(Cell::new(usize::MAX));
    let feeder = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        early.set(asked.get());
        feed(&f1, "one").await;
    };
    within(futures_util::future::join3(a.turn(Some("go".to_string().into())), answers, feeder)).await;
    assert!(mu_base::message::check_tool_pairs(&a.messages.to_vec()).is_ok());
    (results(&a), early.get())
}

#[test]
fn a_call_that_needs_approval_runs_after_the_reads_before_it() {
    run(async {
        let (r, asked_early) = asking_read_after_a_running_read(allow_once()).await;
        assert_eq!(asked_early, 0, "asked while the read before it was still running");
        assert!(r.len() == 2 && r[0].1.contains("one") && !r[1].0 && r[1].1.contains("outside"), "{r:?}");
    });
}

#[test]
fn a_denied_call_keeps_its_place_in_the_results() {
    run(async {
        let (r, asked_early) =
            asking_read_after_a_running_read(ApprovalDecision::Deny { note: Some("no".into()) }).await;
        assert_eq!(asked_early, 0);
        assert!(r.len() == 2 && r[0].1.contains("one") && r[1].0 && r[1].1.contains("denied"), "{r:?}");
    });
}

/// The guard is for subagents only, and a budget of 0 turns it off.
#[test]
fn a_budget_needs_a_subagent_and_a_nonzero_ceiling() {
    run(async {
        let forever = || Ok(assistant(vec![call("t", "read")], StopReason::ToolUse));
        let (mut top, _rx) = agent_run(
            (0..10).map(|_| forever()).collect(),
            Configs::default(),
            RunConfig { request_budget: 1, budget_notice: true, ..Default::default() },
        );
        top.turn(Some("go".to_string().into())).await;
        assert!(
            !top.messages
                .iter()
                .any(|m| matches!(m, Message::User { content, .. } if text_of(content).contains("[budget notice]")))
        );

        let (mut off, _rx) = agent_run(
            (0..10).map(|_| forever()).collect(),
            Configs::default(),
            RunConfig { subagent: true, request_budget: 0, budget_notice: true, ..Default::default() },
        );
        off.turn(Some("go".to_string().into())).await;
        assert!(
            !off.messages
                .iter()
                .any(|m| matches!(m, Message::User { content, .. } if text_of(content).contains("[budget notice]")))
        );
    });
}
