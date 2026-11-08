//! The subagent child protocol: running a `mu --headless` child and reading its result, and the
//! result of a background command. Jobs and `task` both use it.
use super::task::depth;
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
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

/// Characters of a child's result the parent reads inline (omp's `FULL_OUTPUT_THRESHOLD`); past
/// this the head is shown and the whole text goes to an `artifact://` file.
pub const RESULT_PREVIEW_CHARS: usize = 5000;
/// Names the next artifact of the parent session: its number and path.
pub type ArtifactAlloc<'a> = &'a dyn Fn() -> (u32, std::path::PathBuf);

/// What the parent reads of a child's result: `text` itself while it is short, else its head plus a
/// pointer to the whole text. `cwd` scopes the redaction, so a secret the child echoed never lands
/// in the artifact.
pub fn preview_result(text: &str, cwd: &Path, alloc: impl FnOnce() -> (u32, std::path::PathBuf)) -> String {
    let chars = text.chars().count();
    if chars <= RESULT_PREVIEW_CHARS {
        return text.to_string();
    }
    let end = text.char_indices().nth(RESULT_PREVIEW_CHARS).map_or(text.len(), |(i, _)| i);
    let head = &text[..end];
    // Prefer a line boundary, unless the first one is near the top (pretty-printed JSON would
    // preview as a lone `{`).
    let head = match head.rfind('\n') {
        Some(i) if i >= end / 2 => &head[..i],
        _ => head,
    };
    let (id, path) = alloc();
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, mu_base::secrets::redact(text, cwd).text))
        .is_ok();
    let lines = text.lines().count();
    let kb = text.len().div_ceil(1024);
    let more = chars - head.chars().count();
    let note = if saved {
        format!("[…{more} more characters; the whole result ({lines} lines, {kb} KB) is at artifact://{id}]")
    } else {
        format!("[…{more} more characters; this result could not be saved]")
    };
    format!("{head}\n\n{note}")
}

/// One line of a child's stdout: a progress event, or the final result.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ChildEvent {
    Tool {
        #[serde(default)]
        name: String,
        #[serde(default)]
        summary: String,
    },
    Session {
        #[serde(default)]
        path: String,
    },
    Model {
        #[serde(default)]
        label: String,
    },
    /// A tool call needs approval; the parent answers with an `approval` line on stdin.
    Approve {
        id: u64,
        request: ApprovalRequest,
    },
    Result(ChildResult),
}

#[derive(Deserialize)]
struct ChildResult {
    #[serde(default, rename = "isError")]
    is_error: bool,
    #[serde(default)]
    text: String,
    data: Option<Value>,
    #[serde(rename = "schemaOk")]
    schema_ok: Option<bool>,
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
    run_child_observed(exe, cwd, payload, cancel, on_tool, |_| {}, |_| {}, None).await
}

/// `run_child` that also reports the child's session file to `on_session(path)` as soon as the child
/// has opened it, and its model label to `on_model` (older children never say, so either may not run).
/// `approve` answers the child's approval requests; `None` means nobody can.
#[allow(clippy::too_many_arguments)]
pub async fn run_child_observed(
    exe: &Path,
    cwd: &Path,
    payload: &Value,
    cancel: &CancellationToken,
    mut on_tool: impl FnMut(&str, &str),
    mut on_session: impl FnMut(&str),
    mut on_model: impl FnMut(&str),
    approve: Option<&dyn Fn(ApprovalRequest) -> oneshot::Receiver<ApprovalDecision>>,
) -> Outcome {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--headless")
        .current_dir(cwd)
        .env("MU_TASK_DEPTH", (depth() + 1).to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match mu_base::process::spawn_grouped(&mut cmd, true) {
        Ok(c) => c,
        Err(e) => return Outcome::failed(format!("cannot start subagent: {e}")),
    };
    let pid = child.id().unwrap_or(0) as i32;
    let mut stdin = child.stdin.take().unwrap();
    // The task payload, then one line per approval answer, for as long as the child reads them.
    let (wtx, mut wrx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let task = payload.to_string();
    tokio::task::spawn_local(async move {
        let _ = stdin.write_all(task.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        while let Some(line) = wrx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                break;
            }
        }
        let _ = stdin.shutdown().await;
    });
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
    let mut result: Option<ChildResult> = None;
    let mut cancelled = false;
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        tokio::select! {
            n = stdout.read_until(b'\n', &mut line) => match n {
                Ok(0) => break,
                Ok(_) => {
                    // Bytes, not `lines()`: a stray invalid UTF-8 byte must not end the read.
                    match serde_json::from_slice::<ChildEvent>(&line) {
                        Ok(ChildEvent::Tool { name, summary }) => on_tool(&name, &summary),
                        Ok(ChildEvent::Session { path }) => on_session(&path),
                        Ok(ChildEvent::Model { label }) => on_model(&label),
                        Ok(ChildEvent::Approve { id, request }) => {
                            let rx = match approve {
                                Some(f) => f(request),
                                None => {
                                    let (tx, rx) = oneshot::channel();
                                    let _ = tx.send(ApprovalDecision::Unavailable);
                                    rx
                                }
                            };
                            let out = wtx.clone();
                            tokio::task::spawn_local(async move {
                                let d = rx.await.unwrap_or(ApprovalDecision::Deny { note: None });
                                let _ = out.send(json!({"type": "approval", "id": id, "decision": d}).to_string());
                            });
                        }
                        Ok(ChildEvent::Result(r)) => result = Some(r),
                        Err(_) => {}
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
    drop(wtx);
    // A child that ignores everything must not hang the parent: bound the wait, then kill.
    if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await.is_err() {
        mu_base::process::kill_group(pid, libc::SIGKILL);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), child.wait()).await;
    }
    let stderr_tail = err_task.await.unwrap_or_default();
    outcome(result, payload, cancelled, stderr_tail)
}

/// What the child's final result, or its lack of one, means to the caller.
fn outcome(result: Option<ChildResult>, payload: &Value, cancelled: bool, stderr_tail: String) -> Outcome {
    match result {
        Some(r) => {
            let ok = !r.is_error;
            let text = r.text;
            match r.data {
                Some(d) if ok && r.schema_ok != Some(false) => {
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
    pub meta: Option<mu_base::tool::BashMeta>,
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    /// omp's envelope: a short result as is, a long one as its head plus a pointer to the artifact.
    #[test]
    fn a_long_result_is_previewed_and_saved() {
        let d = std::env::temp_dir().join(format!("mu-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("7.log");
        let alloc = || (7, path.clone());
        let short = "x".repeat(RESULT_PREVIEW_CHARS);
        assert_eq!(preview_result(&short, &d, alloc), short, "a result at the budget is shown whole");

        let long: String = (0..400).map(|i| format!("line {i} of the report\n")).collect();
        let text = preview_result(&long, &d, alloc);
        assert!(text.len() < long.len(), "the preview is shorter");
        assert!(text.contains("the whole result (400 lines"), "{}", text);
        assert!(text.contains("artifact://7"), "{}", text);
        assert!(text.starts_with("line 0 of the report\n"), "the head is a line boundary: {text}");
        assert!(!text.contains("line 399"), "the tail is left to the artifact");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), long, "the artifact holds the whole text");
        std::fs::remove_dir_all(&d).ok();
    }

    /// A secret in a child's result must not reach the artifact the parent is pointed at.
    #[test]
    fn the_artifact_is_redacted() {
        let d = std::env::temp_dir().join(format!("mu-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        mu_base::secrets::register_stored("PREVIEW_TEST_KEY", "Qz9-preview-test-value");
        let path = d.join("8.log");
        let long = format!("{}{}\n", "x".repeat(RESULT_PREVIEW_CHARS), "Qz9-preview-test-value");
        let _ = preview_result(&long, &d, || (8, path.clone()));
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("Qz9-preview"), "{saved}");
        assert!(saved.contains("[REDACTED:PREVIEW_TEST_KEY]"), "{saved}");
        std::fs::remove_dir_all(&d).ok();
    }
}
