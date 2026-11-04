use super::subagent::run_child_observed;
use super::{ToolCtx, Tools};
use futures_util::future::join_all;
use mu_base::events::{JobKind, SubagentState, SubagentStatus};
use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// Hard cap on tasks per call and on running background jobs.
pub const MAX_TASKS: usize = 8;

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
    /// JSON Schema of the result; the subagent then returns validated `data`.
    #[serde(rename = "outputSchema")]
    output_schema: Option<Value>,
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
    "Spawn subagents in the background; job ids return immediately. Results arrive automatically as a <system-notice> message and wake an idle session: keep working or end your turn. Never poll, sleep or loop for them. A task's `cwd` (absolute existing directory, e.g. a git worktree) sets where it runs; the default is this session's directory.",
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
            "cwd":{"type":"string","description":"absolute path of an existing directory the task runs in (e.g. a git worktree); defaults to this session's directory"},
            "outputSchema":{"type":"object","description":"JSON Schema for this task's result. The subagent must submit matching JSON via `yield`; you get it back as a ```json block. Omit for a free-text answer."}},"required":["task"]}}},"required":["tasks"]})
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

impl Tools {
    pub async fn task(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        if a.tasks.is_empty() || a.tasks.len() > MAX_TASKS {
            return ToolOutput::err("task: provide 1-8 tasks");
        }
        if let Some(e) =
            a.tasks.iter().filter_map(|t| t.output_schema.as_ref()).find_map(|s| mu_base::schema::check_schema(s).err())
        {
            return ToolOutput::err(format!("task: {e}"));
        }
        let schemas: Vec<Option<&Value>> = a.tasks.iter().map(|t| t.output_schema.as_ref()).collect();
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
        let payload = |agent: &str, task: &str, schema: Option<&Value>| {
            let mut p = json!({"task": task, "context": a.context, "agent": agent, "parentSession": parent});
            if let Some(s) = schema {
                p["outputSchema"] = s.clone();
            }
            p
        };

        if let Some(jobs) = &self.jobs {
            if let Err(e) = jobs.check_capacity(specs.len()) {
                return ToolOutput::err(e);
            }
            let mut started = Vec::new();
            for (i, (name, agent, task, cwd)) in specs.iter().enumerate() {
                let id = jobs.unique_id(name);
                jobs.spawn(id.clone(), agent.clone(), exe.clone(), cwd.clone(), payload(agent, task, schemas[i]));
                started.push(format!("{id} ({agent})"));
            }
            return ToolOutput::ok(format!(
                "Started {} background job(s): {}\nResults arrive automatically as a system notice. Keep working or end your turn; never poll, sleep or loop for them.",
                started.len(),
                started.join(", ")
            ));
        }

        let rows = RefCell::new(
            specs
                .iter()
                .map(|(n, a, t, c)| SubagentStatus {
                    name: n.clone(),
                    agent: a.clone(),
                    tools: 0,
                    last: String::new(),
                    state: SubagentState::Running,
                    kind: JobKind::Subagent,
                    cwd: c.clone(),
                    task: t.clone(),
                    started: std::time::SystemTime::now(),
                    ended: None,
                    delivered: true,
                    note: None,
                    session: None,
                    model: None,
                })
                .collect::<Vec<_>>(),
        );
        let futs = specs.iter().enumerate().map(|(i, (_, agent, task, cwd))| {
            let (rows, exe) = (&rows, &exe);
            let payload = payload(agent, task, schemas[i]);
            async move {
                let out = run_child_observed(
                    exe,
                    cwd,
                    &payload,
                    &ctx.cancel,
                    |name, summary| {
                        let mut r = rows.borrow_mut();
                        r[i].tools += 1;
                        r[i].last = format!("{name} {summary}");
                    },
                    |_| {},
                    |model| rows.borrow_mut()[i].model = Some(model.to_string()),
                )
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
        res.details = Some(json!({"subagents": rows.borrow().iter().zip(&outs).map(|(r, o)| {
            let mut s = json!({"name": r.name, "agent": r.agent, "model": r.model, "tools": r.tools});
            if let Some(d) = &o.data {
                s["data"] = d.clone();
            }
            s
        }).collect::<Vec<_>>()}));
        res
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
    use crate::tools::subagent::run_child;
    use tokio_util::sync::CancellationToken;

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

    #[tokio::test(flavor = "current_thread")]
    async fn the_childs_session_line_reaches_the_observer_before_the_result() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-task-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        let script = "#!/bin/sh\ncat <<'EOF'\n{\"type\":\"model\",\"label\":\"smol · a/b\"}\n{\"type\":\"session\",\"path\":\"/s/child.jsonl\"}\n{\"type\":\"tool\",\"name\":\"read\",\"summary\":\"x\"}\n{\"type\":\"result\",\"text\":\"ok\"}\nEOF\n";
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let seen = RefCell::new(Vec::new());
        let out = tokio::task::LocalSet::new()
            .run_until(run_child_observed(
                &exe,
                &base,
                &json!({}),
                &CancellationToken::new(),
                |n, _| seen.borrow_mut().push(format!("tool {n}")),
                |p| seen.borrow_mut().push(format!("session {p}")),
                |m| seen.borrow_mut().push(format!("model {m}")),
            ))
            .await;
        assert!(out.ok, "{}", out.text);
        assert_eq!(*seen.borrow(), ["model smol · a/b", "session /s/child.jsonl", "tool read"]);
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod typed_tests {
    use super::*;
    use crate::tools::subagent::{Outcome, run_child};
    use tokio_util::sync::CancellationToken;

    async fn child(line: &str, payload: Value) -> Outcome {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-task-typed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, format!("#!/bin/sh\ncat <<'EOF'\n{line}\nEOF\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = tokio::task::LocalSet::new()
            .run_until(run_child(&exe, &base, &payload, &CancellationToken::new(), |_, _| {}))
            .await;
        let _ = std::fs::remove_dir_all(&base);
        out
    }

    #[test]
    fn schema_accepts_an_optional_output_schema_per_task() {
        let s = schema(&[]);
        assert_eq!(s["properties"]["tasks"]["items"]["properties"]["outputSchema"]["type"], "object");
        assert_eq!(s["properties"]["tasks"]["items"]["required"], json!(["task"]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn data_is_returned_as_a_json_block_and_as_value() {
        let o = child(
            r#"{"type":"result","text":"x","isError":false,"data":{"n":1},"schemaOk":true}"#,
            json!({"outputSchema":{}}),
        )
        .await;
        assert!(o.ok);
        assert_eq!(o.data, Some(json!({"n": 1})));
        assert!(o.text.starts_with("```json\n") && o.text.contains("\"n\": 1"), "{}", o.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn old_format_result_still_gives_text() {
        let o = child(r#"{"type":"result","text":"plain","isError":false}"#, json!({})).await;
        assert_eq!((o.ok, o.text.as_str(), o.data), (true, "plain", None));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_schema_task_without_data_fails() {
        let o = child(r#"{"type":"result","text":"prose","isError":false}"#, json!({"outputSchema":{"type":"object"}}))
            .await;
        assert!(!o.ok && o.data.is_none());
        assert!(o.text.contains("no schema-validated data"), "{}", o.text);
        let o = child(
            r#"{"type":"result","text":"no valid result submitted","isError":true,"schemaOk":false}"#,
            json!({"outputSchema":{}}),
        )
        .await;
        assert!(!o.ok);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_output_schema_is_rejected_before_spawning() {
        let t = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into() };
        let a: Args = serde_json::from_value(json!({"tasks":[{"task":"x","outputSchema":{"type":"bogus"}}]})).unwrap();
        let out = t.task(a, &ctx).await;
        assert!(out.is_error && out.text.contains("outputSchema"), "{}", out.text);
    }
}
