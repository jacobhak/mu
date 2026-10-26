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

/// `--headless`: one subagent run. NDJSON on stdout: a `tool` line per tool call, then one `result`.
pub async fn subagent_mode(
    prompt: String,
    cmd: UnboundedSender<Command>,
    mut ev: UnboundedReceiver<AgentEvent>,
) -> i32 {
    use serde_json::json;
    let _ = cmd.send(Command::Prompt(prompt.into()));
    let _ = cmd.send(Command::Exit);
    let (mut buf, mut last) = (String::new(), String::new());
    let (mut is_error, mut session, mut usage) = (false, None::<String>, (0u64, 0u64));
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
                println!("{}", json!({"type": "tool", "name": name, "summary": summary(&name, &args)}));
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
            AgentEvent::TurnEnd { aborted: true } => is_error = true,
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
    println!(
        "{}",
        json!({"type": "result", "text": last.trim_end(), "isError": is_error, "session": session, "usage": {"input": usage.0, "output": usage.1}})
    );
    i32::from(is_error)
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
        assert_eq!(rt.block_on(subagent_mode("x".into(), ctx, erx)), 1);
        let (ctx, _crx) = unbounded_channel();
        let (etx, erx) = unbounded_channel();
        drop(etx);
        let inbox = crate::events::new_inbox();
        assert_eq!(rt.block_on(print_mode("x".into(), ctx, erx, inbox, None)), 1);
    }
}
