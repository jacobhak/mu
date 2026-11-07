use super::*;
use crate::agent::RunConfig;
use crate::testing::{
    agent, agent_run, agent_with, assistant, call, drain, git_repo, results, save_plan_in, tmp_dir, yield_call,
};
use mu_base::config::Configs;
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
        assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
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
fn truncated_reply_with_calls_stays_paired() {
    run(async {
        let script = vec![
            Ok(assistant(vec![call("t1", "read")], StopReason::Length)),
            Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
        ];
        let (mut a, _rx) = agent(script);
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
        a.turn(Some("again".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
        assert!(a.messages.iter().any(|m| matches!(m, Message::ToolResult { is_error: true, .. })));
    });
}

#[test]
fn plan_prompt_inlines_plan_and_path() {
    let p = ExecPlan {
        content: "do it\n".into(),
        title: "T".into(),
        uri: "local://t-plan.md".into(),
        decision: PlanDecision::Compact { model: None },
        base: None,
        approved_at: 0,
        stored: None,
    };
    let s = plan_prompt(&p);
    assert!(s.contains("<plan path=\"local://t-plan.md\">\ndo it\n</plan>") && s.contains("History usable"));
    assert!(!s.contains("{{"));
}

#[test]
fn failed_turn_clears_stale_exec_plan() {
    run(async {
        let (mut a, _rx) = agent(vec![Err(ProviderError::Fatal("nope".into()))]);
        a.plan.exec_plan = Some(ExecPlan {
            content: "plan".into(),
            title: String::new(),
            uri: String::new(),
            decision: PlanDecision::Execute { model: None },
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
        for _ in 0..100 {
            if jobs.has_undelivered() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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
        assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
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
        assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
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
        let sleep = Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": "sleep 1"}) };
        let script = vec![
            Ok(assistant(vec![sleep, call("t2", "__panic")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = agent(script);
        let inbox = a.ports.inbox.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            inbox.borrow_mut().stop = true;
        });
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
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
        let sleep = Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": "sleep 5"}) };
        let script = vec![Ok(assistant(vec![sleep, call("t2", "__panic")], StopReason::ToolUse))];
        let (mut a, mut rx) = agent(script);
        let inbox = a.ports.inbox.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            inbox.borrow_mut().stop = true;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            inbox.borrow().cancel.cancel();
        });
        a.turn(Some("go".to_string().into())).await;
        assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
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
        drive(tokio::time::sleep(std::time::Duration::from_millis(50)), &mut q).await;
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
fn a_wait_while_a_job_runs_is_refused_and_ends_the_turn() {
    use mu_tools::tools::subagent::BackgroundResult;
    run(async {
        let bash = |id: &str, cmd: &str| Block::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: json!({"command": cmd}),
        };
        let script = vec![
            Ok(assistant(vec![bash("w1", "sleep 60; echo"), bash("t2", "basename /x/ran")], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
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
        assert!(r[0].0 && r[0].1.contains("Not run: this command waits, and background job bg_"), "{:?}", r[0]);
        assert!(!r[1].0 && r[1].1.contains("text: \"ran"), "the rest of the batch still runs: {:?}", r[1]);
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no request after the refusal");
        assert!(drain(&mut rx).iter().any(|e| matches!(e, AgentEvent::ToolSkipped { id } if id == "w1")));
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

fn experimental_agent(script: Vec<Result<Message, ProviderError>>) -> (Agent, UnboundedReceiver<AgentEvent>) {
    let mut cfg = Configs::default();
    cfg.mu.experimental.interruptible_wait = true;
    agent_with(script, cfg)
}

fn bash_call(id: &str, args: serde_json::Value) -> Block {
    Block::ToolCall { id: id.into(), name: "bash".into(), arguments: args }
}

#[test]
fn wait_is_offered_only_with_the_experiment_and_jobs() {
    run(async {
        let names = |a: &Agent| a.tool_defs().into_iter().map(|d| d.name).collect::<Vec<_>>();
        let (mut off, _rx) = agent(vec![]);
        off.enable_jobs();
        assert!(!names(&off).contains(&"wait".to_string()), "the experiment is off");

        let (mut on, _rx) = experimental_agent(vec![]);
        assert!(!names(&on).contains(&"wait".to_string()), "no jobs, no wait");
        on.enable_jobs();
        assert!(names(&on).contains(&"wait".to_string()));

        let bash = |a: &Agent| a.tool_defs().into_iter().find(|d| d.name == "bash").unwrap().description;
        assert!(bash(&off).contains("The shell persists across calls"), "flag off keeps the persistent shell");
        assert!(bash(&on).contains("Each call runs in its own shell"), "flag on describes isolated calls");
        assert!(!bash(&on).contains("Never wait for a job in the foreground"));
    });
}

#[test]
fn with_the_experiment_a_waiting_command_runs_and_no_end_of_turn_is_prescribed() {
    use mu_tools::tools::subagent::BackgroundResult;
    run(async {
        let script = vec![
            Ok(assistant(vec![bash_call("w1", json!({"command": "printf 'waited\\n'"}))], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
        ];
        let (mut a, _rx) = experimental_agent(script);
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
        a.messages.push(Message::user(mu_base::ask_nudge::REMINDER));
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
        a.messages.push(Message::user("go"));
        a.messages.push(assistant(vec![Block::text("Keep it or drop it?")], StopReason::Stop));
        a.messages.push(Message::user(mu_base::ask_nudge::REMINDER));
        a.messages.push(assistant(vec![Block::text(".")], StopReason::Stop));
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
            decision: PlanDecision::Execute { model: None },
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
        let sleep =
            Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": "sleep 0.3"}) };
        let script = vec![
            Ok(assistant(vec![sleep], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("one")], StopReason::Stop)),
        ];
        let (mut a, mut rx) = notify_agent(script);
        a.ensure_session().await;
        let id = a.session_id();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            enqueue(&id, "steward", "also HELLO").unwrap();
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
fn parking_a_plan_saves_it_and_leaves_plan_mode() {
    run(async {
        let args = json!({"slug": "t", "title": "T"});
        let call = Block::ToolCall { id: "p1".into(), name: "propose_plan".into(), arguments: args };
        let (mut a, mut rx) = agent(vec![
            Ok(assistant(vec![call], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("parked")], StopReason::Stop)),
        ]);
        a.cwd = tmp_dir();
        a.enter_plan().await;
        let r = a.tools.resolve("local://t-plan.md").unwrap();
        std::fs::create_dir_all(r.path.parent().unwrap()).unwrap();
        std::fs::write(&r.path, "do it\n").unwrap();
        let answer = async {
            let mut parked = None;
            while let Some(e) = rx.recv().await {
                match e {
                    AgentEvent::PlanReview { reply, stored, .. } => {
                        assert!(!stored);
                        let _ = reply.send(PlanDecision::Park);
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
        assert!(a.plan.plan.is_none());
        let store = plans::store_for(&a.cwd).await;
        let (meta, text) = store.load("t").unwrap();
        assert_eq!((meta.status, text.as_str()), (mu_base::events::Status::Pending, "do it\n"));
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
            decision: PlanDecision::Keep { model: None },
            base: crate::review::facts::snapshot_tree(&a.cwd).await.ok(),
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
        a.messages.push(Message::synthetic_user("plan"));
        a.entry_ids.push("p".into());
        a.messages.push(Message::user("<system-reminder>\nnote\n</system-reminder>"));
        a.entry_ids.push("r".into());
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
        let start = a.start_plan("t", PlanDecision::Keep { model: None }, false).await;
        assert!(start.is_none());
        assert_eq!(a.ports.providers.scripted.borrow().len(), 1, "no request was made");
        let mut stale = false;
        while let Ok(e) = rx.try_recv() {
            stale |= matches!(e, AgentEvent::Error(m) if m.contains("is stale"));
        }
        assert!(stale);
        assert!(a.start_plan("t", PlanDecision::Keep { model: None }, true).await.is_some());
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
        a.costs.quota.enabled = true;
        a.costs.quota.set_usage(mu_base::events::ProviderUsage { provider: "anthropic".into(), windows: vec![] });
        a.model.model = ModelSpec::gateway("anthropic/m", 200_000, 8192);
        a.model.model.native = true;
        a.sync_model_state();
        assert!(a.costs.quota.wait() >= std::time::Duration::from_secs(3600), "the run loop must not spin");
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
