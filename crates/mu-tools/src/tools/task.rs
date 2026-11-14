use super::subagent::{ArtifactAlloc, ChildCmd, Mirror, Outcome, prepare_launch, preview_result, run_child_observed};
use super::{ToolCtx, Tools};
use crate::permissions::Permissions;
use futures_util::future::join_all;
use mu_base::events::{EventTx, JobKind, SubagentState, SubagentStatus};
use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Hard cap on tasks per call and on running background jobs.
pub const MAX_TASKS: usize = 8;

/// omp's bundled caps: a scout answers a slice, so its budget is half a worker's; a configured
/// budget can lower these but never raise them.
const BUNDLED_REQUEST_BUDGET: &[(&str, u32)] = &[("scout", 100), ("sonic", 100)];
/// The bundled cap for any other agent.
const DEFAULT_REQUEST_BUDGET: u32 = 200;

/// The soft request budget of `agent`: `configured` (`[task] soft_request_budget`), lowered to the
/// agent's bundled cap. 0 turns the guard off.
pub fn soft_request_budget(agent: &str, configured: u32) -> u32 {
    if configured == 0 {
        return 0;
    }
    let bundled = BUNDLED_REQUEST_BUDGET
        .iter()
        .find(|(name, _)| *name == agent)
        .map_or(DEFAULT_REQUEST_BUDGET, |(_, budget)| *budget);
    configured.min(bundled)
}

#[derive(Deserialize)]
pub struct Args {
    context: Option<String>,
    tasks: Vec<T>,
}

#[derive(Deserialize)]
struct T {
    name: Option<String>,
    task: String,
    /// How open-ended the child's problem is. Required, as in omp: a spawn that cannot say what is
    /// already decided tends to hand over an open-ended survey instead of a slice.
    #[serde(rename = "solutionSpace")]
    _solution_space: String,
    agent: Option<String>,
    model: Option<String>,
    cwd: Option<String>,
    /// JSON Schema of the result; the subagent then returns validated `data`.
    #[serde(rename = "outputSchema")]
    output_schema: Option<Value>,
}

macro_rules! guidance {
    () => {
        " Each task runs in a fresh context with its own tools and returns its final message.\n\n# Agents\n- `task` (default): can read, edit and run commands; prefer one agent to investigate and edit its slice.\n- `scout`: read-only (read, grep, glob, web); for research over code you have not mapped yet.\n\n# Delegation\nFan genuinely independent slices out in one `tasks[]` batch; do shared prerequisites inline and sequence only true dependencies. Shared edits need one integration owner; set interfaces in `context`. Tell tasks to skip build/lint/tests mid-flight and run them once afterward. Agents start blank, so never reference the conversation.\n\n# Format\n`context`: shared background (`# Goal`, `# Contract` interfaces); do not repeat it per task.\n`name`: CamelCase label.\n`solutionSpace`: how open-ended the child's problem is, whether the fix or design is given or which causes or designs remain open; volume of work does not widen it (`one fix: rename, names given`; `deadlock cause open, no repro`). NEVER name sibling agents or coordination.\n`task`: self-contained (`# Target` files and non-goals, `# Change` steps and APIs, `# Acceptance` observable result)."
    };
}

pub const DESCRIPTION: &str = concat!(
    "Run subagents in parallel and wait for all of them. A task's `cwd` (absolute existing directory, e.g. a git worktree) sets where it runs; the default is this session's directory.",
    guidance!()
);

pub const ASYNC_WAIT_DESCRIPTION: &str = concat!(
    "Spawn subagents in the background; job ids return immediately. Their results arrive as <system-notice> messages when they finish. Use `wait` when a job's result is needed now: it returns when one of the jobs finishes, and a new prompt from the user stops the wait without stopping the work. Stop a job with `job_cancel`. A task's `cwd` (absolute existing directory, e.g. a git worktree) sets where it runs; the default is this session's directory.",
    guidance!()
);

pub fn schema(names: &[&str], roles: &[&str]) -> Value {
    let mut agents = vec!["task", "scout"];
    agents.extend_from_slice(names);
    let mut item = json!({"type":"object","properties":{
        "name":{"type":"string"},
        "task":{"type":"string"},
        "agent":{"type":"string","enum":agents},
        "solutionSpace":{"type":"string","description":"How open-ended the child's problem is: the fix or design given, or which causes or designs remain open. Volume of work does not widen it."},
        "cwd":{"type":"string","description":"absolute path of an existing directory the task runs in (e.g. a git worktree); defaults to this session's directory"},
        "outputSchema":{"type":"object","description":"JSON Schema for this task's result. The subagent must submit matching JSON via `yield`; you get it back as a ```json block. Omit for a free-text answer."}},"required":["task","solutionSpace"]});
    if !roles.is_empty() {
        item["properties"]["model"] = json!({
            "type": "string",
            "enum": roles,
            "description": "Model role for this task; omit to run on the agent's model. Roles are listed under Delegation in the system prompt."
        });
    }
    json!({"type":"object","properties":{
        "context":{"type":"string","description":"shared background prepended to every task"},
        "tasks":{"type":"array","minItems":1,"maxItems":8,"items":item}},"required":["tasks"]})
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
        let plan = ctx.plan;
        let delegated = self.delegated.borrow().clone();
        // (name, agent, task, cwd, model)
        let specs: Vec<(String, String, String, PathBuf, Option<String>)> = match a
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| -> Result<_, String> {
                let agent = if plan { "scout" } else { t.agent.as_deref().unwrap_or("task") };
                let cwd = resolve_cwd(t.cwd.as_deref(), &self.cwd)?;
                let model =
                    t.model.as_deref().map(|m| if m.starts_with('@') { m.to_string() } else { format!("@{m}") });
                if let Some(m) = &model
                    && !delegated.contains(m)
                {
                    let list = if delegated.is_empty() {
                        "no model roles are offered".to_string()
                    } else {
                        format!("offered: {}", delegated.join(", "))
                    };
                    return Err(format!("task: model `{m}` is not offered; {list}"));
                }
                Ok((
                    t.name.clone().unwrap_or_else(|| format!("Task{}", i + 1)),
                    agent.to_string(),
                    t.task.clone(),
                    cwd,
                    model,
                ))
            })
            .collect::<Result<_, String>>()
        {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(e),
        };
        let parent = self.session.path.borrow().clone();
        let session = self.session_env().into_iter().find(|(k, _)| *k == "PI_SESSION_ID").map(|(_, v)| v);
        let session = session.unwrap_or_default();
        #[cfg(any(test, feature = "test-support"))]
        let self_exe = self.child_exe.clone().map_or_else(mu_common::paths::self_exe, Ok);
        #[cfg(not(any(test, feature = "test-support")))]
        let self_exe = mu_common::paths::self_exe();
        let exe = match self_exe {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(format!("cannot locate mu executable: {e}")),
        };
        // A launched child's session file is on another machine, and a mirrored worker's children's
        // lines must reach its own parent: both ask their child to send its lines.
        let mirror = if self.mirror {
            Mirror::Forward(ctx.events.clone())
        } else if self.launcher.is_empty() {
            Mirror::Off
        } else {
            Mirror::Write
        };
        let payload = |agent: &str, task: &str, schema: Option<&Value>, model: Option<&str>| {
            let mut p = json!({"task": task, "context": a.context, "agent": agent, "parentSession": parent});
            p["permissions"] = self.permissions.inherit_json();
            if self.mirror || !self.launcher.is_empty() {
                p["mirrorSession"] = json!(true);
            }
            if let Some(s) = schema {
                p["outputSchema"] = s.clone();
            }
            if let Some(m) = model {
                p["model"] = json!(m);
            }
            p
        };

        if let Some(jobs) = &self.jobs {
            if let Err(e) = jobs.check_capacity(specs.len()) {
                return ToolOutput::err(e);
            }
            // Ids and launches first: a launch that fails then starts nothing, and drops the others'
            // task refs.
            let mut ids: Vec<String> = Vec::new();
            let mut children = Vec::new();
            for (name, _, _, cwd, _) in &specs {
                let id = (1..)
                    .map(|n| if n == 1 { name.clone() } else { format!("{name}-{n}") })
                    .find(|c| jobs.unique_id(c) == *c && !ids.contains(c))
                    .unwrap_or_default();
                children.push(if self.launcher.is_empty() {
                    ChildCmd::Local(exe.clone())
                } else {
                    match prepare_launch(&self.launcher, cwd, &id, &session).await {
                        Ok(l) => ChildCmd::Launched(l),
                        Err(e) => return ToolOutput::err(e),
                    }
                });
                ids.push(id);
            }
            let mut started = Vec::new();
            for (i, ((_, agent, task, cwd, model), child)) in specs.iter().zip(children).enumerate() {
                let id = ids[i].clone();
                jobs.spawn(
                    id.clone(),
                    agent.clone(),
                    child,
                    cwd.clone(),
                    payload(agent, task, schemas[i], model.as_deref()),
                    self.permissions.clone(),
                    self.artifact_dir(),
                    mirror.clone(),
                );
                started.push(format!("{id} ({agent})"));
            }
            let next = "When each finishes, mu automatically starts your next turn with its result; use `wait` when you need a result now. Until then, do other work that does not depend on them.";
            return ToolOutput::ok(format!(
                "Started {} background job(s): {}\n{next}",
                started.len(),
                started.join(", ")
            ));
        }
        let alloc = || self.next_artifact();
        let how = Starter { exe: &exe, launcher: &self.launcher, session: &session, mirror };
        run_foreground(&specs, &schemas, &how, &payload, &ctx.cancel, &ctx.events, &self.permissions, &self.cwd, &alloc)
            .await
    }
}

/// What starts a foreground task's child: this executable, or the launcher, which needs a launch
/// prepared per task.
struct Starter<'a> {
    exe: &'a Path,
    launcher: &'a [String],
    /// The parent's session id.
    session: &'a str,
    mirror: Mirror,
}

/// Builds one child's task payload.
type Payload<'a> = dyn Fn(&str, &str, Option<&Value>, Option<&str>) -> Value + 'a;

/// Runs every task to completion in this call; each row's progress is reported in `details`, and an
/// approval a task needs is asked in the parent's UI.
#[allow(clippy::too_many_arguments)]
async fn run_foreground(
    specs: &[(String, String, String, PathBuf, Option<String>)],
    schemas: &[Option<&Value>],
    how: &Starter<'_>,
    payload: &Payload<'_>,
    cancel: &tokio_util::sync::CancellationToken,
    events: &EventTx,
    perms: &Rc<Permissions>,
    cwd: &Path,
    alloc: ArtifactAlloc<'_>,
) -> ToolOutput {
    let rows = RefCell::new(
        specs
            .iter()
            .map(|(n, a, t, c, _)| SubagentStatus {
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
    let futs = specs.iter().enumerate().map(|(i, (name, agent, task, cwd, model))| {
        let rows = &rows;
        let payload = payload(agent, task, schemas[i], model.as_deref());
        let label = format!("{name} ({agent})");
        async move {
            let child = if how.launcher.is_empty() {
                ChildCmd::Local(how.exe.to_path_buf())
            } else {
                match prepare_launch(how.launcher, cwd, name, how.session).await {
                    Ok(l) => ChildCmd::Launched(l),
                    Err(e) => {
                        rows.borrow_mut()[i].state = SubagentState::Failed;
                        return Outcome::failed(e);
                    }
                }
            };
            let out = run_child_observed(
                child,
                cwd,
                &payload,
                cancel,
                |name, summary| {
                    let mut r = rows.borrow_mut();
                    r[i].tools += 1;
                    r[i].last = format!("{name} {summary}");
                },
                |_| {},
                |model| rows.borrow_mut()[i].model = Some(model.to_string()),
                Some(&|r| perms.forward(events, &label, r)),
                how.mirror.clone(),
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
        .map(|((n, a, _, _, _), o)| {
            format!("## {n} ({a}) — {}\n{}", if o.ok { "ok" } else { "failed" }, preview_result(&o.text, cwd, alloc))
        })
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

#[cfg(test)]
mod schema_tests {
    #[test]
    fn schema_enum_includes_custom_agents() {
        let s = super::schema(&["x"], &[]);
        assert_eq!(
            s["properties"]["tasks"]["items"]["properties"]["agent"]["enum"],
            serde_json::json!(["task", "scout", "x"])
        );
    }

    #[test]
    fn the_model_field_is_offered_only_with_roles() {
        let s = super::schema(&[], &["@w"]);
        assert_eq!(s["properties"]["tasks"]["items"]["properties"]["model"]["enum"], serde_json::json!(["@w"]));
        assert!(super::schema(&[], &[])["properties"]["tasks"]["items"]["properties"].get("model").is_none());
    }

    #[test]
    fn tasks_is_required_at_the_top_level() {
        let s = super::schema(&[], &[]);
        assert_eq!(s["required"], serde_json::json!(["tasks"]));
        assert!(s["properties"]["tasks"].get("required").is_none());
    }
}

#[cfg(test)]
mod cwd_tests {
    use super::*;
    use crate::tools::subagent::run_child;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn schema_accepts_an_optional_cwd_per_task() {
        let s = schema(&[], &[]);
        assert_eq!(s["properties"]["tasks"]["items"]["properties"]["cwd"]["type"], "string");
        assert_eq!(s["properties"]["tasks"]["items"]["required"], json!(["task", "solutionSpace"]));
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
                ChildCmd::Local(exe),
                &base,
                &json!({}),
                &CancellationToken::new(),
                |n, _| seen.borrow_mut().push(format!("tool {n}")),
                |p| seen.borrow_mut().push(format!("session {p}")),
                |m| seen.borrow_mut().push(format!("model {m}")),
                None,
                Mirror::Off,
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
        let s = schema(&[], &[]);
        assert_eq!(s["properties"]["tasks"]["items"]["properties"]["outputSchema"]["type"], "object");
        assert_eq!(s["properties"]["tasks"]["items"]["required"], json!(["task", "solutionSpace"]));
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

    /// omp requires the spawn to say how open the child's problem is: no declaration, no spawn.
    #[test]
    fn a_task_must_declare_its_solution_space() {
        let missing = serde_json::from_value::<Args>(json!({"tasks":[{"task":"x"}]}));
        let err = missing.err().expect("solutionSpace is required").to_string();
        assert!(err.contains("solutionSpace"), "{err}");
        let ok = serde_json::from_value::<Args>(json!({"tasks":[{"task":"x","solutionSpace":"one fix: given"}]}));
        assert!(ok.is_ok());
    }

    #[test]
    fn a_budget_lowers_but_never_raises_the_bundled_cap() {
        assert_eq!(soft_request_budget("scout", 200), 100, "a scout is capped at 100");
        assert_eq!(soft_request_budget("task", 200), 200);
        assert_eq!(soft_request_budget("scout", 50), 50, "a lower configured budget applies");
        assert_eq!(soft_request_budget("task", 300), 200, "a worker never rises above 200");
        assert_eq!(soft_request_budget("other", 200), 200, "an unnamed agent gets the default");
        assert_eq!(soft_request_budget("scout", 0), 0, "0 turns the guard off");
    }

    /// Echoes the task payload it read so the test can see it.
    const ECHO_CHILD: &str = r#"#!/bin/sh
IFS= read -r line
escaped=$(printf '%s' "$line" | sed 's/"/\\"/g')
printf '{"type":"result","text":"%s"}\n' "$escaped"
"#;

    fn ctx() -> ToolCtx {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false, wait_interrupt: None }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_task_model_must_be_offered_and_reaches_the_child_payload() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-task-model-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, ECHO_CHILD).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut t = Tools::for_test();
        t.child_exe = Some(exe);
        let args = || -> Args {
            serde_json::from_value(json!({"tasks":[{"task":"x","solutionSpace":"one fix: given","model":"@nope"}]}))
                .unwrap()
        };
        let out = t.task(args(), &ctx()).await;
        assert!(out.is_error && out.text.contains("not offered; no model roles are offered"), "{}", out.text);
        *t.delegated.borrow_mut() = vec!["@w".into()];
        let out = t.task(args(), &ctx()).await;
        assert!(out.is_error && out.text.contains("not offered; offered: @w"), "{}", out.text);

        let a: Args =
            serde_json::from_value(json!({"tasks":[{"task":"x","solutionSpace":"one fix: given","model":"w"}]}))
                .unwrap();
        let out = tokio::task::LocalSet::new().run_until(t.task(a, &ctx())).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains(r#""model":"@w""#), "{}", out.text);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn only_a_mirrored_parent_asks_its_local_child_for_the_session_lines() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-task-mirror-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, ECHO_CHILD).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut t = Tools::for_test();
        t.child_exe = Some(exe);
        let args = || -> Args {
            serde_json::from_value(json!({"tasks":[{"task":"x","solutionSpace":"one fix: given"}]})).unwrap()
        };
        let out = tokio::task::LocalSet::new().run_until(t.task(args(), &ctx())).await;
        assert!(!out.text.contains("mirrorSession"), "{}", out.text);
        t.mirror = true;
        let out = tokio::task::LocalSet::new().run_until(t.task(args(), &ctx())).await;
        assert!(out.text.contains(r#""mirrorSession":true"#), "{}", out.text);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_launch_prepare_is_that_rows_failure() {
        let mut t = Tools::for_test();
        t.launcher = vec!["/nonexistent/launcher".into()];
        let dir = mu_testutil::tmp();
        let a: Args = serde_json::from_value(
            json!({"tasks":[{"task":"x","solutionSpace":"one fix: given","cwd":dir.to_str().unwrap()}]}),
        )
        .unwrap();
        let out = t.task(a, &ctx()).await;
        assert!(out.is_error && out.text.contains("needs a git repository"), "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_output_schema_is_rejected_before_spawning() {
        let t = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx =
            ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false, wait_interrupt: None };
        let a: Args = serde_json::from_value(
            json!({"tasks":[{"task":"x","solutionSpace":"one fix: given","outputSchema":{"type":"bogus"}}]}),
        )
        .unwrap();
        let out = t.task(a, &ctx).await;
        assert!(out.is_error && out.text.contains("outputSchema"), "{}", out.text);
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    use crate::tools::subagent::run_child_observed;
    use mu_base::permissions::{ApprovalDecision, ApprovalRequest, ApprovalScope};
    use std::os::unix::fs::PermissionsExt;
    use tokio_util::sync::CancellationToken;

    /// A child that reads its task line, asks for approval, then echoes the answer it got back.
    const CHILD: &str = r#"#!/bin/sh
IFS= read -r task
printf '%s\n' '{"type":"approve","id":1,"request":{"tool":"bash","call_id":"c","title":"bash: touch x","reason":"ask mode: bash needs approval","body":"touch x","from":null,"grant":null}}'
IFS= read -r line
escaped=$(printf '%s' "$line" | sed 's/"/\\"/g')
printf '{"type":"result","text":"%s"}\n' "$escaped"
"#;

    fn fake(base: &Path) -> PathBuf {
        std::fs::create_dir_all(base).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, CHILD).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    async fn run(
        base: &Path,
        answer: Option<&dyn Fn(ApprovalRequest) -> tokio::sync::oneshot::Receiver<ApprovalDecision>>,
    ) -> String {
        run_child_observed(
            ChildCmd::Local(fake(base)),
            base,
            &json!({"task": "go"}),
            &CancellationToken::new(),
            |_, _| {},
            |_| {},
            |_| {},
            answer,
            Mirror::Off,
        )
        .await
        .text
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_approval_reaches_the_parent_and_the_answer_reaches_the_child() {
        let base = std::env::temp_dir().join(format!("mu-task-approve-{}", uuid::Uuid::new_v4()));
        let seen = std::cell::RefCell::new(Vec::new());
        let answer = |r: ApprovalRequest| {
            seen.borrow_mut().push(r.title.clone());
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None, patterns: None });
            rx
        };
        let out = tokio::task::LocalSet::new().run_until(run(&base, Some(&answer))).await;
        assert!(out.contains("\"decision\":\"allow\""), "{out}");
        assert_eq!(*seen.borrow(), ["bash: touch x"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn without_a_forwarder_the_child_is_told_nobody_can_approve() {
        let base = std::env::temp_dir().join(format!("mu-task-approve-{}", uuid::Uuid::new_v4()));
        let out = tokio::task::LocalSet::new().run_until(run(&base, None)).await;
        assert!(out.contains("\"decision\":\"unavailable\""), "{out}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
