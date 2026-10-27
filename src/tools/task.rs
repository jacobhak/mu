use super::{ToolCtx, ToolOutput, Tools};
use crate::events::{SubagentState, SubagentStatus};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
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
    cwd: Option<String>,
}

macro_rules! guidance {
    () => {
        " Each task runs in a fresh context with its own tools and returns its final message.\n\n# Agents\n- `task` (default): can read, edit and run commands; prefer one agent to investigate and edit its slice.\n- `scout`: read-only (read, grep, glob, web); for research over code you have not mapped yet.\n\n# Delegation\nFan genuinely independent slices out in one `tasks[]` batch; do shared prerequisites inline and sequence only true dependencies. Shared edits need one integration owner; set interfaces in `context`. Tell tasks to skip build/lint/tests mid-flight and run them once afterward. Agents start blank, so never reference the conversation.\n\n# Format\n`context`: shared background (`# Goal`, `# Contract` interfaces); do not repeat it per task.\n`name`: CamelCase label.\n`task`: self-contained (`# Target` files and non-goals, `# Change` steps and APIs, `# Acceptance` observable result)."
    };
}

pub const DESCRIPTION: &str = concat!(
    "Run subagents in parallel and wait for all of them. A task's `cwd` (absolute existing directory, e.g. a git worktree) sets where it runs; the default is this session's directory.",
    guidance!()
);

pub const ASYNC_DESCRIPTION: &str = concat!(
    "Spawn subagents in the background; job ids return immediately. Results arrive automatically as a <system-notice> message: keep working, never poll, and call `wait` only when blocked with nothing else to do. A task's `cwd` (absolute existing directory, e.g. a git worktree) sets where it runs; the default is this session's directory.",
    guidance!()
);

pub fn schema(names: &[&str]) -> Value {
    let mut agents = vec!["task", "scout"];
    agents.extend_from_slice(names);
    json!({"type":"object","properties":{
        "context":{"type":"string","description":"shared background prepended to every task"},
        "tasks":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","properties":{
            "name":{"type":"string"},
            "task":{"type":"string"},
            "agent":{"type":"string","enum":agents},
            "cwd":{"type":"string","description":"absolute path of an existing directory the task runs in (e.g. a git worktree); defaults to this session's directory"}},"required":["task"]}}},"required":["tasks"]})
}

pub fn depth() -> u32 {
    std::env::var("MU_TASK_DEPTH").ok().and_then(|d| d.parse().ok()).unwrap_or(0)
}

/// The directory a task runs in: its `cwd` when given (absolute and existing), else `default`.
fn resolve_cwd(raw: Option<&str>, default: &Path) -> Result<PathBuf, String> {
    let Some(raw) = raw else { return Ok(default.to_path_buf()) };
    let p = Path::new(raw);
    if !p.is_absolute() {
        return Err(format!("task: cwd must be an absolute path: {raw}"));
    }
    if !p.is_dir() {
        return Err(format!("task: cwd does not exist or is not a directory: {raw}"));
    }
    Ok(p.to_path_buf())
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
        // (name, agent, task, cwd)
        let specs: Vec<(String, String, String, PathBuf)> = match a
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| -> Result<_, String> {
                let agent = if plan { "scout" } else { t.agent.as_deref().unwrap_or("task") };
                let cwd = resolve_cwd(t.cwd.as_deref(), &self.cwd)?;
                Ok((t.name.clone().unwrap_or_else(|| format!("Task{}", i + 1)), agent.to_string(), t.task.clone(), cwd))
            })
            .collect::<Result<_, String>>()
        {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(e),
        };
        let parent = self.session_path.borrow().clone();
        let exe = match mu_common::paths::self_exe() {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(format!("cannot locate mu executable: {e}")),
        };
        let payload = |agent: &str, task: &str| json!({"task": task, "context": a.context, "agent": agent, "parentSession": parent});

        if let Some(jobs) = &self.jobs {
            let running = jobs.running();
            if running + specs.len() > MAX_TASKS {
                return ToolOutput::err(format!(
                    "too many background jobs running ({running}/{MAX_TASKS}); wait for some to finish"
                ));
            }
            let mut started = Vec::new();
            for (name, agent, task, cwd) in &specs {
                let id = jobs.unique_id(name);
                jobs.spawn(id.clone(), agent.clone(), exe.clone(), cwd.clone(), payload(agent, task));
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
                .map(|(n, a, _, _)| SubagentStatus {
                    name: n.clone(),
                    agent: a.clone(),
                    tools: 0,
                    last: String::new(),
                    state: SubagentState::Running,
                })
                .collect::<Vec<_>>(),
        );
        let futs = specs.iter().enumerate().map(|(i, (_, agent, task, cwd))| {
            let (rows, exe) = (&rows, &exe);
            let payload = payload(agent, task);
            async move {
                let out = run_child(exe, cwd, &payload, &ctx.cancel, |name, summary| {
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
            .map(|((n, a, _, _), o)| format!("## {n} ({a}) — {}\n{}", if o.ok { "ok" } else { "failed" }, o.text))
            .collect::<Vec<_>>()
            .join("\n\n");
        let mut res = if all_failed { ToolOutput::err(text) } else { ToolOutput::ok(text) };
        res.details = Some(
            json!({"subagents": rows.borrow().iter().map(|r| json!({"name": r.name, "agent": r.agent, "tools": r.tools})).collect::<Vec<_>>()}),
        );
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
    #[cfg(target_os = "linux")]
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
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut result: Option<Value> = None;
    let mut cancelled = false;
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        tokio::select! {
            n = stdout.read_until(b'\n', &mut line) => match n {
                Ok(0) => break,
                Ok(_) => {
                    // Bytes, not `lines()`: a stray invalid UTF-8 byte must not end the read.
                    match serde_json::from_slice::<Value>(&line) {
                        Ok(v) if v["type"] == "tool" => on_tool(v["name"].as_str().unwrap_or(""), v["summary"].as_str().unwrap_or("")),
                        Ok(v) if v["type"] == "result" => result = Some(v),
                        _ => {}
                    }
                }
                Err(_) => {
                    // The pipe is broken: stop the child instead of leaving it running unobserved.
                    if pid > 0 {
                        tokio::task::spawn_local(crate::proc::terminate_group(pid));
                    }
                    break;
                }
            },
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                if pid > 0 {
                    tokio::task::spawn_local(crate::proc::terminate_group(pid));
                }
            }
        }
    }
    // A child that ignores everything must not hang the parent: bound the wait, then kill.
    if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await.is_err() {
        if pid > 0 {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), child.wait()).await;
    }
    let stderr_tail = err_task.await.unwrap_or_default();
    match result {
        Some(v) => {
            Outcome { ok: !v["isError"].as_bool().unwrap_or(false), text: v["text"].as_str().unwrap_or("").to_string() }
        }
        None => Outcome {
            ok: false,
            text: if cancelled {
                "cancelled".into()
            } else if stderr_tail.is_empty() {
                "subagent exited without a result".into()
            } else {
                stderr_tail
            },
        },
    }
}

#[cfg(test)]
mod schema_tests {
    #[test]
    fn schema_enum_includes_custom_agents() {
        let s = super::schema(&["x"]);
        assert_eq!(
            s["properties"]["tasks"]["items"]["properties"]["agent"]["enum"],
            serde_json::json!(["task", "scout", "x"])
        );
    }
}

#[cfg(test)]
mod cwd_tests {
    use super::*;

    #[test]
    fn schema_accepts_an_optional_cwd_per_task() {
        let s = schema(&[]);
        assert_eq!(s["properties"]["tasks"]["items"]["properties"]["cwd"]["type"], "string");
        assert_eq!(s["properties"]["tasks"]["items"]["required"], json!(["task"]));
    }

    #[test]
    fn resolve_cwd_defaults_and_rejects_relative_or_missing_paths() {
        let d = std::env::temp_dir();
        assert_eq!(resolve_cwd(None, &d).unwrap(), d);
        assert_eq!(resolve_cwd(d.to_str(), Path::new("/")).unwrap(), d);
        assert!(resolve_cwd(Some("relative/dir"), &d).unwrap_err().contains("absolute"));
        assert!(resolve_cwd(Some("/definitely/not/a/dir-mu"), &d).unwrap_err().contains("does not exist"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_runs_in_the_given_dir() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-task-cwd-{}", std::process::id()));
        let dir = base.join("work");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, "#!/bin/sh\nprintf '{\"type\":\"result\",\"text\":\"%s\"}\\n' \"$(pwd -P)\"\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = tokio::task::LocalSet::new()
            .run_until(run_child(&exe, &dir, &json!({}), &CancellationToken::new(), |_, _| {}))
            .await;
        assert!(out.ok, "{}", out.text);
        assert_eq!(out.text, dir.canonicalize().unwrap().display().to_string());
        let _ = std::fs::remove_dir_all(&base);
    }
}
