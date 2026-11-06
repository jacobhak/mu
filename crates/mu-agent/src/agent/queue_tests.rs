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
        drive(tokio::time::sleep(Duration::from_millis(20)), &mut q).await;
        assert!(inbox.borrow().steer.is_empty() && inbox.borrow().follow_up.is_empty());
        let evs = drain(&mut erx);
        assert!(
            evs.iter().any(|e| matches!(e, AgentEvent::QueuedDropped(d) if texts(d) == ["s", "f"])),
            "what the turn did not take is reported, steer first"
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
                if let AgentEvent::PlanReview { reply: r, .. } = e {
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
        assert!(a.plan.plan.is_none());
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
        drive(tokio::time::sleep(Duration::from_millis(20)), &mut q).await;
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
        drive(tokio::time::sleep(Duration::from_millis(20)), &mut q).await;
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
