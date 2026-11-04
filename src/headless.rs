use crate::tools::summary;
use mu_base::events::{AgentEvent, Command, PlanDecision};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// `-p`: runs one prompt to completion. Final assistant text goes to stdout, progress to stderr.
/// Each message on `stop` is a request to hand the session over: the first ends the turn at the
/// next boundary (after the running tool, if any), the second cancels the current step. A stopped run
/// exits 3 and prints how to resume. Returns the process exit code.
pub async fn print_mode(
    first: Command,
    expect_park: bool,
    cmd: UnboundedSender<Command>,
    mut ev: UnboundedReceiver<AgentEvent>,
    inbox: mu_base::events::SharedInbox,
    max_cost: Option<f64>,
    stop: UnboundedReceiver<()>,
) -> i32 {
    let mut stop = Some(stop);
    // A stored plan opens its session once it starts, so a refusal leaves none behind.
    if !matches!(first, Command::StartPlan { .. } | Command::RefinePlan { .. }) {
        let _ = cmd.send(Command::EnsureSession);
    }
    let _ = cmd.send(first);
    let _ = cmd.send(Command::Exit);
    let (mut buf, mut last, mut code) = (String::new(), String::new(), 0);
    let (mut cost, mut capped, mut exited, mut errored, mut parked) = (0.0f64, false, false, false, false);
    let (mut session, mut stops) = (None::<String>, 0u32);
    let mut skipped = None::<(String, String)>;
    loop {
        let e = tokio::select! {
            e = ev.recv() => match e {
                Some(e) => e,
                None => break,
            },
            s = async {
                match &mut stop {
                    Some(r) => r.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if s.is_none() {
                    stop = None;
                    continue;
                }
                stops += 1;
                if stops == 1 {
                    inbox.borrow_mut().stop = true;
                    eprintln!("mu: stop requested; stopping at the next boundary");
                } else {
                    inbox.borrow().cancel.cancel();
                    eprintln!("mu: stopping now");
                }
                continue;
            }
        };
        match e {
            AgentEvent::Status(s) => {
                cost = s.cost;
                if let Some(p) = session_line(&mut session, s.session_path) {
                    eprintln!("{p}");
                }
                if let Some(max) = max_cost.filter(|m| s.cost > *m)
                    && !capped
                {
                    capped = true;
                    inbox.borrow().cancel.cancel();
                    eprintln!("mu: cost cap ${max:.2} exceeded");
                    code = 1;
                }
            }
            AgentEvent::Text(t) => buf.push_str(&t),
            AgentEvent::BlockEnd => {
                if !buf.is_empty() {
                    last = std::mem::take(&mut buf);
                }
            }
            AgentEvent::ToolSkipped { id } => skipped = Some((id, String::new())),
            AgentEvent::ToolStart { id, name, args } => match &mut skipped {
                // One line for a call that will not run, printed with its reason at `ToolEnd`.
                Some((sid, head)) if *sid == id => *head = format!("{name} {}", summary(&name, &args)),
                _ => eprintln!("● {name} {}", summary(&name, &args)),
            },
            AgentEvent::ToolEnd { id, output, .. } if skipped.as_ref().is_some_and(|(s, _)| *s == id) => {
                let (_, head) = skipped.take().unwrap_or_default();
                eprintln!("○ {head} (not run: {})", output.text)
            }
            AgentEvent::Retry { attempt, delay, reason } => {
                eprintln!("↻ retrying in {}s ({reason}) [attempt {attempt}/5]", delay.as_secs())
            }
            AgentEvent::Compacted { before, after } => eprintln!("── compacted {before}→{after} tokens"),
            AgentEvent::Notice(s) => eprintln!("{s}"),
            AgentEvent::Error(s) => {
                eprintln!("{s}");
                code = 1;
            }
            AgentEvent::TurnFailed => errored = true,
            AgentEvent::TurnEnd { aborted: true } => code = 1,
            AgentEvent::Exit => {
                exited = true;
                break;
            }
            // Nobody is there to approve: a plan that reaches review is parked for later.
            AgentEvent::PlanReview { reply, .. } => {
                let _ = reply.send(PlanDecision::Park);
            }
            AgentEvent::PlanParked { id, path } => {
                eprintln!("plan: {id} · {}", path.display());
                parked = true;
            }
            _ => {}
        }
    }
    if !exited {
        eprintln!("mu: agent stopped unexpectedly");
        code = 1;
    }
    if !buf.is_empty() {
        last = buf;
    }
    if expect_park && !parked && code == 0 && stops == 0 {
        eprintln!("mu: no plan was parked");
        code = 1;
    }
    if max_cost.is_some() {
        eprintln!("mu: cost ${cost:.4}");
    }
    if stops > 0 && exited && !capped && !errored {
        match &session {
            Some(p) => {
                eprintln!("mu: interrupted; resume with: mu {}--resume {p}", mu_session::session::profile_flag())
            }
            None => eprintln!("mu: interrupted"),
        }
        return 3;
    }
    if code == 0 {
        println!("{}", last.trim_end());
    }
    code
}

/// The `session:` line for the first status that has a session path; `None` ever after.
fn session_line(seen: &mut Option<String>, path: Option<String>) -> Option<String> {
    if seen.is_some() {
        return None;
    }
    let p = path?;
    *seen = Some(p.clone());
    Some(format!("session: {p}"))
}

/// Asked once when a turn ends without a result and the task has an output schema.
const NUDGE: &str = "You have not submitted a result. Call the `yield` tool now with `data` matching the output schema (or `error` if you cannot).";

/// `--headless`: one subagent run. NDJSON on stdout: one `session` line when its session file exists,
/// a `tool` line per tool call, then one `result`.
/// With `output_schema` the run must end in a successful `yield` call; its validated `data` goes
/// in the result as `data` with `schemaOk: true`, and anything else is an error.
pub async fn subagent_mode(
    prompt: String,
    output_schema: Option<serde_json::Value>,
    cmd: UnboundedSender<Command>,
    ev: UnboundedReceiver<AgentEvent>,
) -> i32 {
    let (result, code) = run_subagent(prompt, output_schema.is_some(), cmd, ev, |l| println!("{l}")).await;
    println!("{result}");
    code
}

pub(crate) async fn run_subagent(
    prompt: String,
    structured: bool,
    cmd: UnboundedSender<Command>,
    mut ev: UnboundedReceiver<AgentEvent>,
    out: impl Fn(serde_json::Value),
) -> (serde_json::Value, i32) {
    use serde_json::json;
    let _ = cmd.send(Command::Prompt(prompt.into()));
    // A structured run may need a second prompt, so `Exit` waits for the end of the turn.
    let mut exit_sent = !structured;
    if !structured {
        let _ = cmd.send(Command::Exit);
    }
    let (mut buf, mut last) = (String::new(), String::new());
    let (mut is_error, mut session, mut usage) = (false, None::<String>, (0u64, 0u64));
    let (mut data, mut yield_error, mut nudged) = (None::<serde_json::Value>, None::<String>, false);
    let mut exited = false;
    while let Some(e) = ev.recv().await {
        match e {
            AgentEvent::Text(t) => buf.push_str(&t),
            AgentEvent::BlockEnd => {
                if !buf.is_empty() {
                    last = std::mem::take(&mut buf);
                }
            }
            AgentEvent::ToolStart { name, args, .. } => {
                out(json!({"type": "tool", "name": name, "summary": summary(&name, &args)}));
            }
            AgentEvent::ToolEnd { name, output, .. } if name == "yield" && !output.is_error => {
                match output.details.as_ref().map(|d| (d["status"].as_str(), d)) {
                    Some((Some("success"), d)) => data = d.get("data").cloned(),
                    Some((Some("aborted"), d)) => yield_error = d["error"].as_str().map(str::to_string),
                    _ => {}
                }
            }
            AgentEvent::Status(s) => {
                if let (Some(p), None) = (&s.session_path, &session) {
                    out(json!({"type": "session", "path": p}));
                }
                session = s.session_path.or(session);
                usage = (s.tokens_in, s.tokens_out);
            }
            AgentEvent::Retry { attempt, delay, reason } => {
                eprintln!("retrying in {}s ({reason}) [attempt {attempt}/5]", delay.as_secs())
            }
            AgentEvent::Notice(s) => eprintln!("{s}"),
            AgentEvent::Error(s) => {
                eprintln!("{s}");
                is_error = true;
                if last.is_empty() {
                    last = s;
                }
            }
            AgentEvent::TurnEnd { aborted } => {
                is_error |= aborted;
                if !exit_sent {
                    if data.is_none() && yield_error.is_none() && !is_error && !nudged {
                        nudged = true;
                        let _ = cmd.send(Command::Prompt(NUDGE.into()));
                    } else {
                        exit_sent = true;
                        let _ = cmd.send(Command::Exit);
                    }
                }
            }
            AgentEvent::Exit => {
                exited = true;
                break;
            }
            _ => {}
        }
    }
    if !exited {
        is_error = true;
        if last.is_empty() && buf.is_empty() {
            last = "agent stopped unexpectedly".into();
        }
    }
    if !buf.is_empty() {
        last = buf;
    }
    let mut result = json!({"type": "result", "text": last.trim_end(), "isError": is_error, "session": session, "usage": {"input": usage.0, "output": usage.1}});
    if structured {
        match (&data, &yield_error) {
            (Some(d), _) => {
                result["data"] = d.clone();
                result["schemaOk"] = true.into();
            }
            (None, Some(e)) => {
                result["isError"] = true.into();
                result["schemaOk"] = false.into();
                result["text"] = e.as_str().into();
            }
            (None, None) => {
                result["isError"] = true.into();
                result["schemaOk"] = false.into();
                let why = if is_error { "the run failed" } else { "the agent finished without calling yield" };
                result["text"] = format!("no valid result submitted: {why}\n{}", last.trim_end()).trim_end().into();
            }
        }
    }
    let code = i32::from(result["isError"] == true);
    (result, code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn subagent_without_exit_is_an_error() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (ctx, _crx) = unbounded_channel();
        let (etx, erx) = unbounded_channel();
        drop(etx); // the agent task died before sending Exit
        assert_eq!(rt.block_on(subagent_mode("x".into(), None, ctx, erx)), 1);
        let (ctx, _crx) = unbounded_channel();
        let (etx, erx) = unbounded_channel();
        drop(etx);
        let inbox = mu_base::events::new_inbox();
        let (_stx, srx) = unbounded_channel();
        assert_eq!(rt.block_on(print_mode(Command::Prompt("x".into()), false, ctx, erx, inbox, None, srx)), 1);
    }

    fn status(cost: f64, path: Option<&str>) -> AgentEvent {
        AgentEvent::Status(mu_base::events::StatusSnapshot {
            cost,
            session_path: path.map(String::from),
            ..Default::default()
        })
    }

    /// Runs `print_mode` over scripted events with `stops` stop requests already queued.
    fn drive(events: Vec<AgentEvent>, stops: usize, max_cost: Option<f64>) -> (i32, bool) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (ctx, mut crx) = unbounded_channel();
        let (etx, erx) = unbounded_channel();
        let (stx, srx) = unbounded_channel();
        for _ in 0..stops {
            stx.send(()).unwrap();
        }
        let inbox = mu_base::events::new_inbox();
        let i2 = inbox.clone();
        // Events are fed only after a yield, so the queued stops are handled first.
        let code = rt.block_on(async {
            let feeder = async {
                tokio::task::yield_now().await;
                for e in events {
                    etx.send(e).unwrap();
                    tokio::task::yield_now().await;
                }
            };
            let (code, _) =
                tokio::join!(print_mode(Command::Prompt("x".into()), false, ctx, erx, i2, max_cost, srx), feeder);
            code
        });
        assert!(matches!(crx.try_recv(), Ok(Command::EnsureSession)));
        let stop = inbox.borrow().stop;
        (code, stop)
    }

    #[test]
    fn a_stopped_run_exits_3_and_sets_the_flag() {
        let ev = vec![status(0.0, Some("/s/a_b.jsonl")), status(0.0, Some("/s/a_b.jsonl")), AgentEvent::Exit];
        assert_eq!(drive(ev, 1, None), (3, true));
    }

    #[test]
    fn no_stop_means_a_normal_exit() {
        assert_eq!(drive(vec![status(0.0, Some("/s/a_b.jsonl")), AgentEvent::Exit], 0, None), (0, false));
    }

    #[test]
    fn the_cost_cap_wins_over_a_stop() {
        let ev = vec![status(5.0, Some("/s/a_b.jsonl")), AgentEvent::Exit];
        assert_eq!(drive(ev, 1, Some(1.0)).0, 1);
    }

    #[test]
    fn a_second_stop_cancels() {
        let ev = vec![status(0.0, None), AgentEvent::TurnEnd { aborted: true }, AgentEvent::Exit];
        assert_eq!(drive(ev, 2, None).0, 3);
    }

    #[test]
    fn a_plan_run_parks_the_review_and_needs_a_parked_plan() {
        let run = |events: Vec<AgentEvent>, expect_park: bool| {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            let (ctx, _crx) = unbounded_channel();
            let (etx, erx) = unbounded_channel();
            let (_stx, srx) = unbounded_channel();
            for e in events {
                etx.send(e).unwrap();
            }
            let inbox = mu_base::events::new_inbox();
            rt.block_on(print_mode(Command::TogglePlan(None), expect_park, ctx, erx, inbox, None, srx))
        };
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let review = AgentEvent::PlanReview {
            title: "T".into(),
            uri: "local://t-plan.md".into(),
            file: std::path::PathBuf::new(),
            content: String::new(),
            stored: false,
            note: None,
            reply: tx,
        };
        assert_eq!(run(vec![review, AgentEvent::Exit], true), 1, "nothing was parked");
        assert!(matches!(rx.try_recv(), Ok(PlanDecision::Park)));
        let parked = AgentEvent::PlanParked { id: "t".into(), path: "/p/t.md".into() };
        assert_eq!(run(vec![parked, AgentEvent::Exit], true), 0);
        assert_eq!(run(vec![AgentEvent::Exit], false), 0);
    }

    #[test]
    fn the_session_line_is_printed_once() {
        let mut seen = None;
        assert_eq!(session_line(&mut seen, None), None);
        assert_eq!(session_line(&mut seen, Some("/s/a.jsonl".into())).as_deref(), Some("session: /s/a.jsonl"));
        assert_eq!(session_line(&mut seen, Some("/s/a.jsonl".into())), None);
        assert_eq!(session_line(&mut seen, Some("/s/b.jsonl".into())), None);
    }

    #[test]
    fn a_non_fatal_error_does_not_suppress_exit_3() {
        let ev = vec![
            status(0.0, None),
            AgentEvent::Error("[output truncated: max_tokens reached]".into()),
            AgentEvent::Exit,
        ];
        assert_eq!(drive(ev, 1, None).0, 3);
    }

    #[test]
    fn a_fatal_error_beats_a_stop() {
        let ev =
            vec![status(0.0, None), AgentEvent::Error("error: boom".into()), AgentEvent::TurnFailed, AgentEvent::Exit];
        assert_eq!(drive(ev, 1, None).0, 1);
    }

    fn run(structured: bool, events: Vec<AgentEvent>) -> (serde_json::Value, i32, Vec<Command>) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (ctx, mut crx) = unbounded_channel();
        let (etx, erx) = unbounded_channel();
        for e in events {
            etx.send(e).unwrap();
        }
        let (r, code) = rt.block_on(run_subagent("x".into(), structured, ctx, erx, |_| {}));
        let mut cmds = Vec::new();
        while let Ok(c) = crx.try_recv() {
            cmds.push(c);
        }
        (r, code, cmds)
    }

    #[test]
    fn the_session_line_is_emitted_once_when_the_session_first_appears() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (ctx, _crx) = unbounded_channel();
        let (etx, erx) = unbounded_channel();
        for e in [
            status(0.0, None),
            status(0.0, Some("/s/a.jsonl")),
            AgentEvent::ToolStart { id: "1".into(), name: "read".into(), args: serde_json::json!({"path": "x"}) },
            status(0.1, Some("/s/a.jsonl")),
            AgentEvent::Exit,
        ] {
            etx.send(e).unwrap();
        }
        let lines = std::cell::RefCell::new(Vec::new());
        rt.block_on(run_subagent("x".into(), false, ctx, erx, |l| lines.borrow_mut().push(l)));
        let lines = lines.into_inner();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], serde_json::json!({"type": "session", "path": "/s/a.jsonl"}));
        assert_eq!(lines[1]["type"], "tool");
    }

    fn yielded(details: serde_json::Value) -> AgentEvent {
        let mut output = mu_base::tool::ToolOutput::ok("Result submitted.");
        output.details = Some(details);
        AgentEvent::ToolEnd { id: "1".into(), name: "yield".into(), output }
    }

    #[test]
    fn structured_run_returns_validated_data() {
        let (r, code, _) = run(
            true,
            vec![
                AgentEvent::Text("done".into()),
                yielded(serde_json::json!({"status":"success","data":{"n":1}})),
                AgentEvent::TurnEnd { aborted: false },
                AgentEvent::Exit,
            ],
        );
        assert_eq!(
            (code, &r["data"], &r["schemaOk"], &r["isError"]),
            (0, &serde_json::json!({"n":1}), &true.into(), &false.into())
        );
        assert_eq!(r["text"], "done");
    }

    #[test]
    fn structured_run_without_yield_is_nudged_once_then_fails() {
        let (r, code, cmds) = run(
            true,
            vec![
                AgentEvent::Text("prose".into()),
                AgentEvent::TurnEnd { aborted: false },
                AgentEvent::TurnEnd { aborted: false },
                AgentEvent::Exit,
            ],
        );
        assert_eq!((code, &r["schemaOk"], &r["isError"]), (1, &false.into(), &true.into()));
        assert!(r["data"].is_null() && r["text"].as_str().unwrap().starts_with("no valid result submitted"));
        // Prompt, one nudge, then Exit.
        assert!(matches!(cmds.as_slice(), [Command::Prompt(_), Command::Prompt(_), Command::Exit]));
    }

    #[test]
    fn structured_run_reports_a_yielded_error() {
        let (r, code, _) = run(
            true,
            vec![
                yielded(serde_json::json!({"status":"aborted","error":"cannot"})),
                AgentEvent::TurnEnd { aborted: false },
                AgentEvent::Exit,
            ],
        );
        assert_eq!((code, r["text"].as_str()), (1, Some("cannot")));
    }

    #[test]
    fn plain_run_has_no_data_fields_and_exits_immediately() {
        let (r, code, cmds) = run(false, vec![AgentEvent::Text("hi".into()), AgentEvent::Exit]);
        assert_eq!((code, r["text"].as_str()), (0, Some("hi")));
        assert!(r.get("data").is_none() && r.get("schemaOk").is_none());
        assert!(matches!(cmds.as_slice(), [Command::Prompt(_), Command::Exit]));
    }
}
