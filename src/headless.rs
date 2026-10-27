use crate::events::{AgentEvent, Command};
use crate::tools::summary;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// `-p`: runs one prompt to completion. Final assistant text goes to stdout, progress to stderr.
/// Returns the process exit code.
pub async fn print_mode(
    prompt: String,
    cmd: UnboundedSender<Command>,
    mut ev: UnboundedReceiver<AgentEvent>,
    inbox: crate::events::SharedInbox,
    max_cost: Option<f64>,
) -> i32 {
    let _ = cmd.send(Command::Prompt(prompt.into()));
    let _ = cmd.send(Command::Exit);
    let (mut buf, mut last, mut code) = (String::new(), String::new(), 0);
    let (mut cost, mut capped, mut exited) = (0.0f64, false, false);
    while let Some(e) = ev.recv().await {
        match e {
            AgentEvent::Status(s) => {
                cost = s.cost;
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
            AgentEvent::ToolStart { name, args, .. } => eprintln!("● {name} {}", summary(&name, &args)),
            AgentEvent::Retry { attempt, delay, reason } => {
                eprintln!("↻ retrying in {}s ({reason}) [attempt {attempt}/5]", delay.as_secs())
            }
            AgentEvent::Compacted { before, after } => eprintln!("── compacted {before}→{after} tokens"),
            AgentEvent::Notice(s) => eprintln!("{s}"),
            AgentEvent::Error(s) => {
                eprintln!("{s}");
                code = 1;
            }
            AgentEvent::TurnEnd { aborted: true } => code = 1,
            AgentEvent::Exit => {
                exited = true;
                break;
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
    if max_cost.is_some() {
        eprintln!("mu: cost ${cost:.4}");
    }
    if code == 0 {
        println!("{}", last.trim_end());
    }
    code
}

/// Asked once when a turn ends without a result and the task has an output schema.
const NUDGE: &str = "You have not submitted a result. Call the `yield` tool now with `data` matching the output schema (or `error` if you cannot).";

/// `--headless`: one subagent run. NDJSON on stdout: a `tool` line per tool call, then one `result`.
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

async fn run_subagent(
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
        let inbox = crate::events::new_inbox();
        assert_eq!(rt.block_on(print_mode("x".into(), ctx, erx, inbox, None)), 1);
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

    fn yielded(details: serde_json::Value) -> AgentEvent {
        let mut output = crate::tools::ToolOutput::ok("Result submitted.");
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
