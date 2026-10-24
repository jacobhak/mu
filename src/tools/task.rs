use super::{ToolCtx, ToolOutput, Tools};
use crate::events::{SubagentState, SubagentStatus};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// Hard cap on tasks per call and on running background jobs.
const MAX_TASKS: usize = 8;

#[derive(Deserialize)]
pub struct Args {
    context: Option<String>,
    tasks: Vec<T>,
}

#[derive(Deserialize)]
struct T {
    name: Option<String>,
    task: String,
    agent: Option<String>,
}

pub const DESCRIPTION: &str = "Run subagents in parallel and wait for all of them. Each task runs in a fresh context with its own tools and returns its final message. `scout` agents are read-only (read, grep, glob, web); `task` agents can also edit and run commands. Give every task a self-contained description; optional `context` is prepended to each.";

pub const ASYNC_DESCRIPTION: &str = "Spawn subagents in the background; job ids return immediately. Each task runs in a fresh context with its own tools and returns its final message. `scout` agents are read-only (read, grep, glob, web); `task` agents can also edit and run commands. Results arrive automatically as a <system-notice> message; never poll. Call `wait` only when blocked with nothing else to do. Give every task a self-contained description; optional `context` is prepended to each.";

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "context":{"type":"string","description":"shared background prepended to every task"},
        "tasks":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","properties":{
            "name":{"type":"string"},
            "task":{"type":"string"},
            "agent":{"type":"string","enum":["task","scout"]}},"required":["task"]}}},"required":["tasks"]})
}

pub fn depth() -> u32 {
    std::env::var("MU_TASK_DEPTH").ok().and_then(|d| d.parse().ok()).unwrap_or(0)
}

pub struct Outcome {
    pub ok: bool,
    pub text: String,
}

impl Tools {
    pub async fn task(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        if a.tasks.is_empty() || a.tasks.len() > MAX_TASKS {
            return ToolOutput::err("task: provide 1-8 tasks");
        }
        let plan = self.plan.get();
        // (name, agent, task)
        let specs: Vec<(String, String, String)> = a
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let agent = if plan { "scout" } else { t.agent.as_deref().unwrap_or("task") };
                (t.name.clone().unwrap_or_else(|| format!("Task{}", i + 1)), agent.to_string(), t.task.clone())
            })
            .collect();
        let parent = self.session_path.borrow().clone();
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(format!("cannot locate mu executable: {e}")),
        };
        let payload = |agent: &str, task: &str| json!({"task": task, "context": a.context, "agent": agent, "parentSession": parent});

        if let Some(jobs) = &self.jobs {
            let running = jobs.running();
            if running + specs.len() > MAX_TASKS {
                return ToolOutput::err(format!("too many background jobs running ({running}/{MAX_TASKS}); wait for some to finish"));
            }
            let mut started = Vec::new();
            for (name, agent, task) in &specs {
                let id = jobs.unique_id(name);
                jobs.spawn(id.clone(), agent.clone(), exe.clone(), self.cwd.clone(), payload(agent, task));
                started.push(format!("{id} ({agent})"));
            }
            return ToolOutput::ok(format!(
                "Started {} background job(s): {}\nResults arrive automatically as a system notice; never poll. Call `wait` only when blocked with nothing else to do.",
                started.len(),
                started.join(", ")
            ));
        }

        let rows = RefCell::new(
            specs
                .iter()
                .map(|(n, a, _)| SubagentStatus { name: n.clone(), agent: a.clone(), tools: 0, last: String::new(), state: SubagentState::Running })
                .collect::<Vec<_>>(),
        );
        let futs = specs.iter().enumerate().map(|(i, (_, agent, task))| {
            let (rows, exe) = (&rows, &exe);
            let payload = payload(agent, task);
            async move {
                let out = run_child(exe, &self.cwd, &payload, &ctx.cancel, |name, summary| {
                    let mut r = rows.borrow_mut();
                    r[i].tools += 1;
                    r[i].last = format!("{name} {summary}");
                })
                .await;
                rows.borrow_mut()[i].state = if out.ok { SubagentState::Done } else { SubagentState::Failed };
                out
            }
        });
        let outs = join_all(futs).await;
        let all_failed = outs.iter().all(|o| !o.ok);
        let text = specs
            .iter()
            .zip(&outs)
            .map(|((n, a, _), o)| format!("## {n} ({a}) — {}\n{}", if o.ok { "ok" } else { "failed" }, o.text))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut res = if all_failed { ToolOutput::err(text) } else { ToolOutput::ok(text) };
        res.details = Some(json!({"subagents": rows.borrow().iter().map(|r| json!({"name": r.name, "agent": r.agent, "tools": r.tools})).collect::<Vec<_>>()}));
        res
    }
}

/// Runs one `mu --headless` subagent in `cwd`, reporting each tool call to `on_tool(name, summary)`.
/// `cancel` terminates the child's process group.
pub async fn run_child(
    exe: &Path,
    cwd: &Path,
    payload: &Value,
    cancel: &CancellationToken,
    mut on_tool: impl FnMut(&str, &str),
) -> Outcome {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--headless")
        .current_dir(cwd)
        .env("MU_TASK_DEPTH", (depth() + 1).to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.process_group(0);
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome { ok: false, text: format!("cannot start subagent: {e}") },
    };
    let pid = child.id().unwrap_or(0) as i32;
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(payload.to_string().as_bytes()).await;
    drop(stdin);
    let stderr = child.stderr.take().unwrap();
    let err_task = tokio::task::spawn_local(async move {
        let mut tail: std::collections::VecDeque<String> = Default::default();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if tail.len() == 20 {
                tail.pop_front();
            }
            tail.push_back(l);
        }
        tail.into_iter().collect::<Vec<_>>().join("\n")
    });
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut result: Option<Value> = None;
    let mut cancelled = false;
    loop {
        tokio::select! {
            l = lines.next_line() => match l {
                Ok(Some(l)) => match serde_json::from_str::<Value>(&l) {
                    Ok(v) if v["type"] == "tool" => on_tool(v["name"].as_str().unwrap_or(""), v["summary"].as_str().unwrap_or("")),
                    Ok(v) if v["type"] == "result" => result = Some(v),
                    _ => {}
                },
                _ => break,
            },
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                if pid > 0 {
                    unsafe { libc::kill(-pid, libc::SIGTERM) };
                }
            }
        }
    }
    let _ = child.wait().await;
    let stderr_tail = err_task.await.unwrap_or_default();
    match result {
        Some(v) => Outcome { ok: !v["isError"].as_bool().unwrap_or(false), text: v["text"].as_str().unwrap_or("").to_string() },
        None => Outcome {
            ok: false,
            text: if cancelled { "cancelled".into() } else if stderr_tail.is_empty() { "subagent exited without a result".into() } else { stderr_tail },
        },
    }
}
