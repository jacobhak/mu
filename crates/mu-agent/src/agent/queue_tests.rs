//! The command seam: steer, follow-up, cancel and stop, read by `drive` while a turn runs or by the
//! run loop between turns.

use super::*;
use crate::handle::AgentHandle;
use crate::testing::drain;
use mu_testutil::block_on_local as run;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;

fn queue(rx: UnboundedReceiver<Command>, inbox: &SharedInbox, events: EventTx) -> Queue {
    Queue {
        rx,
        inbox: inbox.clone(),
        backlog: Default::default(),
        jobs: None,
        events,
        live: Rc::new(Cell::new(Thinking::Off)),
        perms: Rc::default(),
    }
}

fn texts(v: &[UserInput]) -> Vec<&str> {
    v.iter().map(|i| i.text.as_str()).collect()
}

#[test]
fn steer_and_follow_up_sent_mid_turn_are_reported_when_it_ends() {
    run(async {
        let (tx, rx) = unbounded_channel();
        let (etx, mut erx) = unbounded_channel();
        let inbox = new_inbox();
        tx.send(Command::Steer("s".into())).unwrap();
        tx.send(Command::FollowUp("f".into())).unwrap();
        let mut q = queue(rx, &inbox, etx);
        // The turn outlives the queued commands, so `drive` reads them while it runs.
        drive(crate::testing::long_turn(), &mut q).await;
        assert!(inbox.borrow().steer.is_empty() && inbox.borrow().follow_up.is_empty());
        let evs = drain(&mut erx);
        assert!(
            evs.iter().any(|e| matches!(e, AgentEvent::QueuedDropped(d) if texts(d) == ["s", "f"])),
            "what the turn did not take is reported, steer first"
        );
    });
}

#[test]
fn thinking_changes_are_published_during_a_turn_and_applied_afterwards() {
    run(async {
        let (mut a, mut events) = crate::testing::agent(vec![]);
        let (tx, rx) = unbounded_channel();
        let mut q = queue(rx, &a.ports.inbox, a.ports.events.clone());
        q.live = a.model.live_thinking.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let observer = async {
            for level in [Thinking::High, Thinking::Low] {
                tx.send(Command::SetThinking(level)).unwrap();
                assert!(matches!(events.recv().await, Some(AgentEvent::ThinkingLevel(t)) if t == level));
                assert_eq!(a.snapshot().thinking, level.as_str());
            }
            done_tx.send(()).unwrap();
        };
        tokio::join!(
            drive(
                async {
                    let _ = done_rx.await;
                },
                &mut q
            ),
            observer
        );
        assert_eq!(q.live.get(), Thinking::Low);
        for level in [Thinking::High, Thinking::Low] {
            let cmd = q.backlog.pop_front().unwrap();
            assert!(matches!(cmd, Command::SetThinking(t) if t == level));
            assert!(a.command(cmd, &mut q).await);
        }
        assert_eq!(a.snapshot().thinking, "low");
    });
}

#[test]
fn rapid_thinking_cycles_follow_command_order_during_a_turn() {
    run(async {
        let (mut a, mut events) = crate::testing::agent(vec![]);
        let (tx, rx) = unbounded_channel();
        let mut q = queue(rx, &a.ports.inbox, a.ports.events.clone());
        q.live = a.model.live_thinking.clone();
        q.live.set(Thinking::Off);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let observer = async {
            tx.send(Command::CycleThinking).unwrap();
            tx.send(Command::CycleThinking).unwrap();
            tx.send(Command::SetThinking(Thinking::High)).unwrap();
            tx.send(Command::CycleThinking).unwrap();
            let expected =
                [Thinking::Off.cycle(), Thinking::Off.cycle().cycle(), Thinking::High, Thinking::High.cycle()];
            for level in expected {
                assert!(matches!(events.recv().await, Some(AgentEvent::ThinkingLevel(t)) if t == level));
            }
            done_tx.send(()).unwrap();
        };
        tokio::join!(
            drive(
                async {
                    let _ = done_rx.await;
                },
                &mut q
            ),
            observer
        );
        for level in [Thinking::Off.cycle(), Thinking::Off.cycle().cycle(), Thinking::High, Thinking::High.cycle()] {
            let cmd = q.backlog.pop_front().unwrap();
            assert!(matches!(cmd, Command::SetThinking(t) if t == level));
            assert!(a.command(cmd, &mut q).await);
        }
        assert_eq!(a.snapshot().thinking, Thinking::High.cycle().as_str());
    });
}

#[test]
fn refresh_status_reads_git_and_emits_without_a_turn() {
    run(async {
        let dir = crate::testing::git_repo();
        let (mut a, mut events) = crate::testing::agent_in(vec![], Default::default(), Default::default(), dir.clone());
        let (_tx, rx) = unbounded_channel();
        let mut q = queue(rx, &a.ports.inbox, a.ports.events.clone());
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        assert!(a.command(Command::RefreshStatus, &mut q).await);
        assert!(
            drain(&mut events).iter().any(
                |event| matches!(event, AgentEvent::Status(s) if s.git.as_ref().is_some_and(|git| git.dirty == 1))
            )
        );
        assert!(a.messages.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    });
}

#[test]
fn refresh_status_waits_in_the_backlog_during_a_turn() {
    run(async {
        let (tx, rx) = unbounded_channel();
        let (events, _erx) = unbounded_channel();
        let inbox = new_inbox();
        let mut q = queue(rx, &inbox, events);
        tx.send(Command::RefreshStatus).unwrap();
        drive(crate::testing::long_turn(), &mut q).await;
        assert!(matches!(q.backlog.pop_front(), Some(Command::RefreshStatus)));
    });
}

#[test]
fn compact_waits_in_the_backlog_during_a_turn() {
    run(async {
        let (tx, rx) = unbounded_channel();
        let (events, _erx) = unbounded_channel();
        let inbox = new_inbox();
        let mut q = queue(rx, &inbox, events);
        tx.send(Command::Compact(Some("focus".into()))).unwrap();
        drive(crate::testing::long_turn(), &mut q).await;
        assert!(matches!(q.backlog.pop_front(), Some(Command::Compact(Some(f))) if f == "focus"));
    });
}

#[test]
fn cancel_drops_a_queued_compact_but_keeps_other_backlog() {
    run(async {
        let (tx, rx) = unbounded_channel();
        let (events, mut erx) = unbounded_channel();
        let inbox = new_inbox();
        let mut q = queue(rx, &inbox, events);
        tx.send(Command::Compact(None)).unwrap();
        tx.send(Command::RefreshStatus).unwrap();
        tx.send(Command::Cancel).unwrap();
        drive(crate::testing::long_turn(), &mut q).await;
        assert!(matches!(q.backlog.pop_front(), Some(Command::RefreshStatus)));
        assert!(q.backlog.is_empty());
        assert!(inbox.borrow().cancel.is_cancelled());
        assert!(drain(&mut erx).iter().any(|e| matches!(e, AgentEvent::Notice(m) if m == "queued compaction dropped")));
    });
}

#[test]
fn idle_thinking_cycles_ignore_intervening_status_events() {
    run(async {
        let (mut a, mut events) = crate::testing::agent(vec![]);
        let (_tx, rx) = unbounded_channel();
        let mut q = queue(rx, &a.ports.inbox, a.ports.events.clone());
        a.set_thinking(Thinking::Off);
        assert!(a.command(Command::CycleThinking, &mut q).await);
        assert!(a.command(Command::SetThinking(Thinking::High), &mut q).await);
        assert!(a.command(Command::CycleThinking, &mut q).await);
        let snapshots: Vec<_> = drain(&mut events)
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::Status(s) => Some(s.thinking),
                _ => None,
            })
            .collect();
        assert_eq!(
            snapshots,
            [Thinking::Off, Thinking::Off.cycle(), Thinking::High, Thinking::High.cycle()]
                .map(|t| t.as_str().to_string())
        );
    });
}

#[test]
fn cancel_closes_an_open_plan_dialog_without_a_decision() {
    run(async {
        let (mut a, mut rx) = crate::testing::agent(vec![]);
        a.cwd = crate::testing::git_repo();
        let store = crate::testing::save_plan_in(&a.cwd, &["a.txt"]).await;
        let (tx, crx) = unbounded_channel();
        let inbox = a.ports.inbox.clone();
        let mut q = queue(crx, &inbox, a.ports.events.clone());
        // The reply stays alive while the dialog is open, so only Cancel can end it.
        let feeder = async {
            let mut reply = None;
            while let Some(e) = rx.recv().await {
                if let AgentEvent::Review { reply: r, .. } = e {
                    reply = Some(r);
                    tx.send(Command::Cancel).unwrap();
                    break;
                }
            }
            reply
        };
        let (ran, _reply) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), a.command(Command::OpenPlan("t".into()), &mut q)),
            feeder
        );
        assert!(ran.expect("the dialog closes on cancel"), "the run loop goes on");
        assert!(a.plan.mode.is_none());
        let (meta, _) = store.load("t").unwrap();
        assert_eq!(meta.status, mu_base::events::Status::Pending);
    });
}

#[test]
fn cancel_aborts_the_turn_in_flight() {
    run(async {
        let (tx, rx) = unbounded_channel();
        let (etx, _erx) = unbounded_channel();
        let inbox = new_inbox();
        let token = inbox.borrow().cancel.clone();
        let mut q = queue(rx, &inbox, etx);
        // The turn only ends once its token is cancelled, so `drive` returns only if Cancel is read.
        let feeder = async { tx.send(Command::Cancel).unwrap() };
        tokio::join!(drive(token.cancelled(), &mut q), feeder);
        assert!(inbox.borrow().cancel.is_cancelled());
    });
}

#[test]
fn stop_sets_the_flag_for_the_next_boundary() {
    run(async {
        let (tx, rx) = unbounded_channel();
        let (etx, _erx) = unbounded_channel();
        let inbox = new_inbox();
        tx.send(Command::Stop).unwrap();
        let mut q = queue(rx, &inbox, etx);
        drive(crate::testing::long_turn(), &mut q).await;
        assert!(inbox.borrow().stop);
    });
}

#[test]
fn pop_queue_takes_the_newest_steer_and_reports_the_rest() {
    run(async {
        let (tx, rx) = unbounded_channel();
        let (etx, mut erx) = unbounded_channel();
        let inbox = new_inbox();
        tx.send(Command::Steer("s".into())).unwrap();
        tx.send(Command::FollowUp("f".into())).unwrap();
        tx.send(Command::PopQueue).unwrap();
        let mut q = queue(rx, &inbox, etx);
        drive(crate::testing::long_turn(), &mut q).await;
        let evs = drain(&mut erx);
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::QueuePopped(p) if p.text == "s")));
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::QueuedDropped(d) if texts(d) == ["f"])));
    });
}

#[test]
fn a_steer_sent_to_an_idle_agent_goes_back_to_the_composer() {
    run(async {
        let (a, ev) = crate::testing::agent(vec![]);
        let (cmd, cmd_rx) = unbounded_channel();
        let task = tokio::task::spawn_local(a.run(cmd_rx));
        let h = AgentHandle { cmd, ev, wake: None, task };
        h.steer("x");
        h.send(Command::Exit);
        let AgentHandle { mut ev, task, .. } = h;
        let mut evs = Vec::new();
        while let Some(e) = ev.recv().await {
            let exit = matches!(e, AgentEvent::Exit);
            evs.push(e);
            if exit {
                break;
            }
        }
        task.await.unwrap();
        let dropped = evs.iter().position(|e| matches!(e, AgentEvent::QueuedDropped(d) if texts(d) == ["x"]));
        let exit = evs.iter().position(|e| matches!(e, AgentEvent::Exit));
        assert!(dropped.is_some() && dropped < exit);
    });
}

/// Runs `cmd` on a fresh agent, sends Cancel once `started` matches an event, then Exit, and
/// returns the events up to Exit. Fails if the command ignores Cancel.
fn cancel_after(cmd: Command, started: fn(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    run(async {
        let (a, mut ev) = crate::testing::agent(vec![]);
        let (tx, cmd_rx) = unbounded_channel();
        let task = tokio::task::spawn_local(a.run(cmd_rx));
        tx.send(cmd).unwrap();
        let mut evs = Vec::new();
        let collect = async {
            while let Some(e) = ev.recv().await {
                if started(&e) {
                    tx.send(Command::Cancel).unwrap();
                    tx.send(Command::Exit).unwrap();
                }
                let exit = matches!(e, AgentEvent::Exit);
                evs.push(e);
                if exit {
                    break;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), collect).await.expect("Cancel did not abort the command");
        task.await.unwrap();
        evs
    })
}

#[test]
fn cancel_aborts_a_running_user_bash() {
    let evs = cancel_after(
        Command::Bash { cmd: "sleep 600".into(), include: false },
        |e| matches!(e, AgentEvent::ToolStart { id, .. } if id == "user-bash"),
    );
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::TurnEnd { aborted: true })));
}

#[test]
fn a_follow_up_that_starts_running_leaves_the_published_queue() {
    use crate::testing::assistant;
    use mu_base::message::{Block, StopReason};
    run(async {
        let (mut a, mut rx) = crate::testing::agent(vec![
            Ok(assistant(vec![Block::text("one")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("two")], StopReason::Stop)),
        ]);
        a.ports.inbox.borrow_mut().follow_up.push_back("later".to_string().into());
        a.turn(Some("go".to_string().into())).await;
        let evs = drain(&mut rx);
        let published = evs.iter().position(|e| matches!(e, AgentEvent::Queue(q) if q.follow_up.is_empty()));
        let prompt = evs.iter().position(|e| matches!(e, AgentEvent::UserPrompt(t) if t == "later"));
        assert!(published.is_some() && published < prompt, "the queue bar drops the follow-up before it runs");
    });
}

/// One end-to-end case: a foreground command waits for its result, the user submits `mk()`, and the
/// command keeps running as a job while the new message is answered first.
fn interruptible_wait_case(cmd: fn() -> Command) {
    use crate::testing::assistant;
    use mu_base::message::{Block, StopReason};
    use serde_json::json;
    run(async {
        let dir = std::env::temp_dir().join(format!("mu-iw-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let release = dir.join("release");
        let unwanted = dir.join("unwanted");
        let wait = format!(
            "printf 'started\\n'; while [ ! -f '{}' ]; do sleep 0.02; done; printf 'finished\\n'",
            release.display()
        );
        let touch = format!("touch '{}'", unwanted.display());
        let bash = |id: &str, command: String| Block::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: json!({"command": command, "cwd": dir.clone()}),
        };
        let script = vec![
            Ok(assistant(vec![bash("b1", wait), bash("b2", touch)], StopReason::ToolUse)),
            Ok(assistant(vec![Block::text("handled next")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("job finished")], StopReason::Stop)),
        ];
        let (mut a, mut ev) = crate::testing::agent(script);
        a.enable_jobs();
        let (tx, rx) = unbounded_channel();
        let mut q = queue(rx, &a.ports.inbox, a.ports.events.clone());
        q.jobs = a.tools.jobs.clone();
        let observer = async {
            while let Some(e) = ev.recv().await {
                if let AgentEvent::JobOutput { tail, .. } = &e
                    && tail.contains("started")
                {
                    tx.send(cmd()).unwrap();
                    break;
                }
            }
        };
        let ((), ()) = tokio::join!(drive(a.turn(Some("go".to_string().into())), &mut q), observer);
        let r = crate::testing::results(&a);
        assert!(!r[0].0 && r[0].1.contains("Command continues as background job bg_"), "{:?}", r[0]);
        assert!(r[1].0 && r[1].1.contains("the user sent a new message"), "{:?}", r[1]);
        assert!(!unwanted.exists(), "the second call of the batch did not run");
        assert_eq!(
            a.messages
                .iter()
                .filter(|m| matches!(m, Message::User { content, .. } if mu_base::message::text_of(content) == "next"))
                .count(),
            1,
            "the new message is delivered once"
        );
        assert!(a.ports.providers.scripted.borrow().len() == 1, "the job had not finished yet");
        assert!(!a.messages.iter().any(
            |m| matches!(m, Message::User { content, .. } if mu_base::message::text_of(content).contains("finished"))
        ));

        std::fs::write(&release, "go").unwrap();
        let jobs = a.tools.jobs.clone().unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while !jobs.has_undelivered() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the backgrounded command finished");
        a.turn(None).await;
        let notices: Vec<String> = a
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User { content, .. } => Some(mu_base::message::text_of(content)),
                _ => None,
            })
            .filter(|t| t.contains("finished"))
            .collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("<system-notice>"));
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn interruptible_wait_prompt_backgrounds_foreground_command() {
    for cmd in [
        (|| Command::Steer("next".to_string().into())) as fn() -> Command,
        || Command::FollowUp("next".to_string().into()),
        || Command::Prompt("next".to_string().into()),
    ] {
        interruptible_wait_case(cmd);
    }
}

/// The model waits on a running job; a prompt releases the wait, the job keeps running, and the
/// wait tool itself reports only its summary.
#[test]
fn interruptible_wait_on_a_job_is_released_by_input_and_ends_cleanly() {
    use crate::testing::assistant;
    use mu_base::message::{Block, StopReason};
    use mu_tools::tools::subagent::BackgroundResult;
    use serde_json::json;
    run(async {
        let dir = std::env::temp_dir().join(format!("mu-iw-wait-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let release = dir.join("release");
        let script = vec![
            Ok(assistant(
                vec![Block::ToolCall { id: "w".into(), name: "wait".into(), arguments: json!({}) }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("handled next")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("job done")], StopReason::Stop)),
        ];
        let (mut a, mut ev) = crate::testing::agent(script);
        a.enable_jobs();
        let jobs = a.tools.jobs.clone().unwrap();
        let gate = release.clone();
        jobs.spawn_bash("marker-gated".into(), dir.clone(), move |_cancel, _sink| async move {
            while !gate.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            BackgroundResult { ok: true, text: "finished".into(), note: "exit 0".into(), meta: None }
        });
        let (tx, rx) = unbounded_channel();
        let mut q = queue(rx, &a.ports.inbox, a.ports.events.clone());
        q.jobs = a.tools.jobs.clone();
        // Queued as steering right away: the guard is registered just before the call, so this
        // reaches it even if `wait` starts a moment later.
        let observer = async {
            while let Some(e) = ev.recv().await {
                if matches!(&e, AgentEvent::ToolStart { name, .. } if name == "wait") {
                    tx.send(Command::Steer("next".to_string().into())).unwrap();
                    break;
                }
            }
        };
        let joined = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(drive(a.turn(Some("go".to_string().into())), &mut q), observer)
        });
        joined.await.expect("the queued steering released the wait");
        let r = crate::testing::results(&a);
        assert!(!r[0].0 && r[0].1.contains("Waiting stopped for your new prompt"), "{:?}", r[0]);
        assert!(!r[0].1.contains("finished"), "the wait does not carry the job's result: {:?}", r[0]);
        assert_eq!(
            a.messages
                .iter()
                .filter(|m| matches!(m, Message::User { content, .. } if mu_base::message::text_of(content) == "next"))
                .count(),
            1,
            "the message is delivered once"
        );
        assert!(!jobs.interrupt_wait(), "the guard is dropped after the call");

        std::fs::write(&release, "go").unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while !jobs.has_undelivered() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the job finished");
        a.turn(None).await;
        let notices: Vec<String> = a
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User { content, .. } => Some(mu_base::message::text_of(content)),
                _ => None,
            })
            .filter(|t| t.contains("finished"))
            .collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("<system-notice>"));
        assert!(a.ports.providers.scripted.borrow().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// Input already queued when the wait is registered steers at once instead of waiting for the job.
#[test]
fn input_queued_before_a_wait_is_registered_relaunches_nothing_and_steers() {
    use crate::testing::assistant;
    use mu_base::message::{Block, StopReason};
    use mu_tools::tools::subagent::BackgroundResult;
    use serde_json::json;
    run(async {
        let script = vec![
            Ok(assistant(
                vec![Block::ToolCall { id: "w".into(), name: "wait".into(), arguments: json!({}) }],
                StopReason::ToolUse,
            )),
            Ok(assistant(vec![Block::text("handled early")], StopReason::Stop)),
        ];
        let (mut a, _ev) = crate::testing::agent(script);
        a.enable_jobs();
        let jobs = a.tools.jobs.clone().unwrap();
        jobs.spawn_bash("never ends".into(), std::env::temp_dir(), |cancel, _| async move {
            cancel.cancelled().await;
            BackgroundResult { ok: false, text: String::new(), note: "cancelled".into(), meta: None }
        });
        a.ports.inbox.borrow_mut().steer.push_back("early".into());
        a.ports.inbox.borrow_mut().follow_up.push_back("later".into());
        let (_tx, rx) = unbounded_channel();
        let mut q = queue(rx, &a.ports.inbox, a.ports.events.clone());
        q.jobs = a.tools.jobs.clone();
        let turn = tokio::time::timeout(Duration::from_secs(20), drive(a.turn(Some("go".to_string().into())), &mut q));
        turn.await.expect("the queued steering released the wait");
        let r = crate::testing::results(&a);
        assert!(!r[0].0 && r[0].1.contains("Waiting stopped for your new prompt"), "{:?}", r[0]);
        let delivered: Vec<String> = a
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User { content, .. } => Some(mu_base::message::text_of(content)),
                _ => None,
            })
            .filter(|t| t == "early" || t == "later")
            .collect();
        assert_eq!(delivered, ["early", "later"], "queued input is steered, steering first");
        assert_eq!(jobs.running(), 1, "the job was never restarted or killed");
        jobs.kill(None);
    });
}

#[test]
fn handle_methods_send_the_matching_commands() {
    run(async {
        let (cmd, mut crx) = unbounded_channel();
        let (_etx, ev) = unbounded_channel();
        let task = tokio::task::spawn_local(async {});
        let h = AgentHandle { cmd, ev, wake: None, task };
        h.steer("a");
        h.follow_up("b");
        h.cancel();
        h.stop();
        assert!(matches!(crx.try_recv(), Ok(Command::Steer(i)) if i.text == "a"));
        assert!(matches!(crx.try_recv(), Ok(Command::FollowUp(i)) if i.text == "b"));
        assert!(matches!(crx.try_recv(), Ok(Command::Cancel)));
        assert!(matches!(crx.try_recv(), Ok(Command::Stop)));
    });
}
