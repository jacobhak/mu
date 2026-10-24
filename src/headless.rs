use crate::events::{AgentEvent, Command};
use crate::tools::summary;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// `-p`: runs one prompt to completion. Final assistant text goes to stdout, progress to stderr.
/// Returns the process exit code.
pub async fn print_mode(prompt: String, cmd: UnboundedSender<Command>, mut ev: UnboundedReceiver<AgentEvent>) -> i32 {
    let _ = cmd.send(Command::Prompt(prompt.into()));
    let _ = cmd.send(Command::Exit);
    let (mut buf, mut last, mut code) = (String::new(), String::new(), 0);
    while let Some(e) = ev.recv().await {
        match e {
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
            AgentEvent::Exit => break,
            _ => {}
        }
    }
    if !buf.is_empty() {
        last = buf;
    }
    if code == 0 {
        println!("{}", last.trim_end());
    }
    code
}

/// `--headless`: one subagent run. NDJSON on stdout: a `tool` line per tool call, then one `result`.
pub async fn subagent_mode(prompt: String, cmd: UnboundedSender<Command>, mut ev: UnboundedReceiver<AgentEvent>) -> i32 {
    use serde_json::json;
    let _ = cmd.send(Command::Prompt(prompt.into()));
    let _ = cmd.send(Command::Exit);
    let (mut buf, mut last) = (String::new(), String::new());
    let (mut is_error, mut session, mut usage) = (false, None::<String>, (0u64, 0u64));
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
            AgentEvent::Exit => break,
            _ => {}
        }
    }
    if !buf.is_empty() {
        last = buf;
    }
    println!("{}", json!({"type": "result", "text": last.trim_end(), "isError": is_error, "session": session, "usage": {"input": usage.0, "output": usage.1}}));
    0
}
