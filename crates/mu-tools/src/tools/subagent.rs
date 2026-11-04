//! The subagent child protocol: running a `mu --headless` child and reading its result, and the
//! result of a background command. Jobs and `task` both use it.
use super::task::depth;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

pub struct Outcome {
    pub ok: bool,
    /// The final message, or the validated `data` as a fenced JSON block when there is any.
    pub text: String,
    /// Validated result of a task given an `outputSchema`.
    pub data: Option<Value>,
}

impl Outcome {
    fn failed(text: impl Into<String>) -> Self {
        Self { ok: false, text: text.into(), data: None }
    }
}

/// Runs one `mu --headless` subagent in `cwd`, reporting each tool call to `on_tool(name, summary)`.
/// `cancel` terminates the child's process group.
pub async fn run_child(
    exe: &Path,
    cwd: &Path,
    payload: &Value,
    cancel: &CancellationToken,
    on_tool: impl FnMut(&str, &str),
) -> Outcome {
    run_child_observed(exe, cwd, payload, cancel, on_tool, |_| {}, |_| {}).await
}

/// `run_child` that also reports the child's session file to `on_session(path)` as soon as the child
/// has opened it, and its model label to `on_model` (older children never say, so either may not run).
pub async fn run_child_observed(
    exe: &Path,
    cwd: &Path,
    payload: &Value,
    cancel: &CancellationToken,
    mut on_tool: impl FnMut(&str, &str),
    mut on_session: impl FnMut(&str),
    mut on_model: impl FnMut(&str),
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
        Err(e) => return Outcome::failed(format!("cannot start subagent: {e}")),
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
                        Ok(v) if v["type"] == "session" => on_session(v["path"].as_str().unwrap_or("")),
                        Ok(v) if v["type"] == "model" => on_model(v["label"].as_str().unwrap_or("")),
                        Ok(v) if v["type"] == "result" => result = Some(v),
                        _ => {}
                    }
                }
                Err(_) => {
                    // The pipe is broken: stop the child instead of leaving it running unobserved.
                    if pid > 0 {
                        tokio::task::spawn_local(mu_base::process::terminate_group(pid));
                    }
                    break;
                }
            },
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                if pid > 0 {
                    tokio::task::spawn_local(mu_base::process::terminate_group(pid));
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
            let ok = !v["isError"].as_bool().unwrap_or(false);
            let text = v["text"].as_str().unwrap_or("").to_string();
            let data = v.get("data").filter(|d| !d.is_null()).cloned();
            match data {
                Some(d) if ok && v["schemaOk"] != false => {
                    let pretty = serde_json::to_string_pretty(&d).unwrap_or_else(|_| d.to_string());
                    Outcome { ok: true, text: format!("```json\n{pretty}\n```"), data: Some(d) }
                }
                // Asked for typed data and none came back: never pass the free text off as the result.
                None if ok && payload.get("outputSchema").is_some_and(|s| !s.is_null()) => {
                    Outcome::failed(format!("subagent returned no schema-validated data\n{text}"))
                }
                _ => Outcome { ok, text, data: None },
            }
        }
        None => Outcome::failed(if cancelled {
            "cancelled".to_string()
        } else if stderr_tail.is_empty() {
            "subagent exited without a result".to_string()
        } else {
            stderr_tail
        }),
    }
}

/// A background command's finished result: the text the model sees, and how it ended.
pub struct BackgroundResult {
    pub ok: bool,
    pub text: String,
    /// "exit 3", "timed out", "cancelled", …
    pub note: String,
}
