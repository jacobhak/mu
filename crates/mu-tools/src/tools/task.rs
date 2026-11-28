use super::relay::{RelayAllow, RelayServer};
use super::subagent::WorkerLaunch;
use super::subagent::{
    ArtifactAlloc, ChildCmd, ChildControl, MU_VERSION, Mirror, Outcome, prepare_launch, preview_result,
    run_child_controlled,
};
use super::{ToolCtx, Tools};
use crate::jobs::{Resumable, SpawnOpts};
use crate::permissions::Permissions;
use futures_util::future::join_all;
use mu_base::events::{EventTx, JobKind, SubagentState, SubagentStatus};
use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// Hard cap on tasks per call and on running background jobs.
pub const MAX_TASKS: usize = 8;
/// Longest `timeout` a task may ask for, in seconds.
const MAX_TIMEOUT: u64 = 7200;

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
    /// Jobs (ids, or names of tasks in this call) that must succeed first.
    after: Option<Vec<String>>,
    /// A finished subagent job whose conversation this task continues.
    resume: Option<String>,
    timeout: Option<u64>,
    #[serde(rename = "maxRequests")]
    max_requests: Option<u32>,
    #[serde(rename = "maxCost")]
    max_cost: Option<f64>,
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
    "Spawn subagents in the background; job ids return immediately. At most 8 jobs run at once; more wait in a queue, and `after` holds a task until the jobs it names succeed and passes it their results. Results arrive as <system-notice> messages, or inline from `wait`. `job_status` shows progress, `job_send` messages a running subagent, `job_cancel` stops one, and `resume` continues a finished subagent with a follow-up. A task's `cwd` (absolute existing directory, e.g. a git worktree) sets where it runs; the default is this session's directory.",
    guidance!()
);

/// `jobs`: background jobs exist, so a task may wait for other jobs (`after`) or continue a finished one (`resume`).
pub fn schema(names: &[&str], roles: &[&str], jobs: bool) -> Value {
    let mut agents = vec!["task", "scout"];
    agents.extend_from_slice(names);
    let mut item = json!({"type":"object","properties":{
        "name":{"type":"string"},
        "task":{"type":"string"},
        "agent":{"type":"string","enum":agents},
        "solutionSpace":{"type":"string","description":"How open-ended the child's problem is: the fix or design given, or which causes or designs remain open. Volume of work does not widen it."},
        "cwd":{"type":"string","description":"absolute path of an existing directory the task runs in (e.g. a git worktree); defaults to this session's directory"},
        "outputSchema":{"type":"object","description":"JSON Schema for this task's result. The subagent must submit matching JSON via `yield`; you get it back as a ```json block. Omit for a free-text answer."},
        "timeout":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT,"description":"Seconds; at the limit the subagent is told to wrap up and is cancelled 2 minutes later"},
        "maxRequests":{"type":"integer","minimum":1,"description":"Lower the soft request budget for this task: told to wrap up at it, stopped at 1.5x"},
        "maxCost":{"type":"number","exclusiveMinimum":0,"description":"USD this task may spend: told to wrap up at 80 %, stopped at the limit"}},"required":["task","solutionSpace"]});
    if jobs {
        item["properties"]["after"] = json!({
            "type": "array",
            "items": {"type": "string"},
            "description": "Job ids, or names of tasks in this call, to wait for. This task starts when they all succeed and gets their results in its context; it is not started if one fails"
        });
        item["properties"]["resume"] = json!({
            "type": "string",
            "description": "Id of a finished subagent job of this session: continue its conversation with `task` as the next prompt, in its directory and with its agent"
        });
    }
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
    /// The models a relayed `agent` may call: what this process was itself allowed to pass on, else
    /// the role chain of the task's model (its `model` role, else the agent's own) and `smol`.
    fn relay_allow(&self, agent: &str, model: Option<&str>) -> RelayAllow {
        if let Some(a) = self.plugins.relay_allow() {
            return a;
        }
        let Some(p) = self.plugins.providers() else { return RelayAllow::default() };
        let role = model.map_or(if agent == "scout" { "scout" } else { "task" }, |m| m.trim_start_matches('@'));
        let extra = self.agent_models.borrow().get(agent).cloned().unwrap_or_default();
        RelayAllow::for_task(&p, role, &extra)
    }

    pub async fn task(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        if a.tasks.is_empty() || a.tasks.len() > MAX_TASKS {
            return ToolOutput::err("task: provide 1-8 tasks");
        }
        if let Some(e) =
            a.tasks.iter().filter_map(|t| t.output_schema.as_ref()).find_map(|s| mu_base::schema::check_schema(s).err())
        {
            return ToolOutput::err(format!("task: {e}"));
        }
        for t in &a.tasks {
            if t.timeout.is_some_and(|s| !(1..=MAX_TIMEOUT).contains(&s)) {
                return ToolOutput::err("task: timeout must be 1-7200 seconds");
            }
            if t.max_requests == Some(0) {
                return ToolOutput::err("task: maxRequests must be at least 1");
            }
            if t.max_cost.is_some_and(|c| c.is_nan() || c <= 0.0) {
                return ToolOutput::err("task: maxCost must be above 0");
            }
            if self.jobs.is_none() && (t.resume.is_some() || t.after.as_ref().is_some_and(|v| !v.is_empty())) {
                return ToolOutput::err("task: `after` and `resume` need an interactive session");
            }
        }
        let schemas: Vec<Option<&Value>> = a.tasks.iter().map(|t| t.output_schema.as_ref()).collect();
        let forced = ctx.mode.as_ref().and_then(|m| m.subagent.clone());
        let delegated = self.delegated.borrow().clone();
        // The finished jobs `resume` continues: they fix the agent and the directory.
        let mut resumed: Vec<Option<Resumable>> = Vec::new();
        for t in &a.tasks {
            let r = match (&t.resume, &self.jobs) {
                (Some(id), Some(jobs)) => match jobs.resumable(id) {
                    Ok(r) => Some(r),
                    Err(e) => return ToolOutput::err(format!("task: resume: {e}")),
                },
                _ => None,
            };
            if let Some(r) = &r
                && resumed.iter().flatten().any(|o| o.session == r.session)
            {
                return ToolOutput::err(format!(
                    "task: resume: `{}` is continued by two tasks of this call",
                    t.resume.as_deref().unwrap_or_default()
                ));
            }
            resumed.push(r);
        }
        // (name, agent, task, cwd, model)
        let specs: Vec<(String, String, String, PathBuf, Option<String>)> = match a
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| -> Result<_, String> {
                let prior = resumed[i].as_ref();
                let id = t.resume.as_deref().unwrap_or_default();
                if let (Some(r), Some(asked)) = (prior, t.agent.as_deref())
                    && r.agent != asked
                {
                    return Err(format!("task: resume: `{id}` ran as `{}`; leave `agent` out", r.agent));
                }
                if let (Some(r), Some(asked)) = (prior, t.cwd.as_deref())
                    && Path::new(asked) != r.cwd
                {
                    return Err(format!("task: resume: `{id}` ran in {}; leave `cwd` out", r.cwd.display()));
                }
                let agent = forced
                    .as_deref()
                    .or(prior.map(|r| r.agent.as_str()))
                    .unwrap_or_else(|| t.agent.as_deref().unwrap_or("task"));
                let cwd = match prior {
                    Some(r) => resolve_cwd(r.cwd.to_str(), &self.cwd)?,
                    None => resolve_cwd(t.cwd.as_deref(), &self.cwd)?,
                };
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
                    t.name.clone().or_else(|| t.resume.clone()).unwrap_or_else(|| format!("Task{}", i + 1)),
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
        #[cfg(any(test, feature = "test-support"))]
        let self_exe = self.child_exe.clone().map_or_else(mu_common::paths::self_exe, Ok);
        #[cfg(not(any(test, feature = "test-support")))]
        let self_exe = mu_common::paths::self_exe();
        let exe = match self_exe {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(format!("cannot locate mu executable: {e}")),
        };
        // The kinds `[task] launch` names run on the remote.
        let remote = |agent: &str| self.launch.as_ref().filter(|l| l.runs(agent));
        // A launched child's session file is on another machine, and a mirrored worker's children's
        // lines must reach its own parent: both ask their child to send its lines.
        let mirror_of = |agent: &str| {
            if self.mirror {
                Mirror::Forward(ctx.events.clone())
            } else if remote(agent).is_some() {
                Mirror::Write
            } else {
                Mirror::Off
            }
        };
        // A launched child has no credentials, and a relayed process has none to lend: the child's
        // model calls go to this process.
        let relayed_here = std::env::var("MU_TASK_RELAY").is_ok_and(|v| v == "1")
            || self.plugins.providers().is_some_and(|p| p.is_relayed());
        let relayed = relayed_here || specs.iter().any(|(_, agent, ..)| remote(agent).is_some());
        let max_cost = self.plugins.providers().map_or(0.0, |p| p.cfg.mu.task.max_cost);
        let relay = relayed.then(|| RelayServer {
            plugins: self.plugins.clone(),
            permissions: self.permissions.clone(),
            max_cost,
            cwd: self.cwd.clone(),
        });
        let relay_tools = if relayed { self.plugins.relay_tools_payload() } else { Vec::new() };
        let payload = |i: usize, agent: &str, task: &str, schema: Option<&Value>, model: Option<&str>| {
            let launched = remote(agent).is_some();
            let mut p = json!({
                "task": task, "context": a.context, "agent": agent, "parentSession": parent, "muVersion": MU_VERSION
            });
            if relayed_here || launched {
                p["relay"] = json!(true);
                p["relayModels"] = json!(self.relay_allow(agent, model));
                if !relay_tools.is_empty() {
                    p["relayTools"] = json!(relay_tools);
                }
            }
            p["permissions"] = self.permissions.inherit_json();
            if self.mirror || launched {
                p["mirrorSession"] = json!(true);
            }
            if launched {
                p["depth"] = json!(depth() + 1);
            }
            if let Some(s) = schema {
                p["outputSchema"] = s.clone();
            }
            if let Some(m) = model {
                p["model"] = json!(m);
            }
            if let Some(n) = a.tasks[i].max_requests {
                p["requestBudget"] = json!(n);
            }
            if let Some(c) = a.tasks[i].max_cost {
                p["maxCost"] = json!(c);
            }
            if let Some(r) = &resumed[i] {
                p["resume"] = json!(r.session);
            }
            p
        };
        let timeouts: Vec<Option<Duration>> = a.tasks.iter().map(|t| t.timeout.map(Duration::from_secs)).collect();

        if let Some(jobs) = &self.jobs {
            if let Err(e) = jobs.check_queue(specs.len()) {
                return ToolOutput::err(format!("task: {e}"));
            }
            let mut ids: Vec<String> = Vec::new();
            for (name, ..) in &specs {
                let id = (1..)
                    .map(|n| if n == 1 { name.clone() } else { format!("{name}-{n}") })
                    .find(|c| jobs.unique_id(c) == *c && !ids.contains(c))
                    .unwrap_or_default();
                ids.push(id);
            }
            let after = match resolve_after(&a.tasks, &specs, &ids, |e| jobs.state_of(e).is_some()) {
                Ok(v) => v,
                Err(e) => return ToolOutput::err(e),
            };
            let mut started = Vec::new();
            for (i, (_, agent, task, cwd, model)) in specs.iter().enumerate() {
                let id = ids[i].clone();
                jobs.spawn(
                    id.clone(),
                    agent.clone(),
                    exe.clone(),
                    cwd.clone(),
                    payload(i, agent, task, schemas[i], model.as_deref()),
                    self.permissions.clone(),
                    self.artifact_dir(),
                    mirror_of(agent),
                    relay.clone(),
                    SpawnOpts { after: after[i].clone(), timeout: timeouts[i], launch: remote(agent).cloned() },
                );
                started.push(format!("{id} ({agent}){}", queued_note(jobs, &id, &after[i])));
            }
            let next = "When each finishes, mu automatically starts your next turn with its result; use `wait` to block on results now, `job_status` to look at progress, and `job_send` to redirect a running one. Until then, do other work that does not depend on them.";
            return ToolOutput::ok(format!(
                "Started {} background job(s): {}\n{next}",
                started.len(),
                started.join(", ")
            ));
        }
        let alloc = || self.next_artifact();
        let how = Starter { exe: &exe, launch: self.launch.as_ref(), mirror: &mirror_of, timeouts: &timeouts };
        run_foreground(
            &specs,
            &schemas,
            &how,
            &payload,
            &ctx.cancel,
            &ctx.events,
            &self.permissions,
            relay.as_ref(),
            &self.cwd,
            &alloc,
        )
        .await
    }
}

/// The ids each task waits for: its `after` entries as the ids of this call's tasks or of existing
/// jobs. Errs on an unknown job, a task waiting for itself and a cycle among this call's tasks.
fn resolve_after(
    tasks: &[T],
    specs: &[(String, String, String, PathBuf, Option<String>)],
    ids: &[String],
    exists: impl Fn(&str) -> bool,
) -> Result<Vec<Vec<String>>, String> {
    let by_name: HashMap<&str, &str> = specs.iter().zip(ids).map(|(s, id)| (s.0.as_str(), id.as_str())).collect();
    let mut resolved: Vec<Vec<String>> = Vec::new();
    for (i, t) in tasks.iter().enumerate() {
        let mut deps: Vec<String> = Vec::new();
        for e in t.after.iter().flatten() {
            let id = match by_name.get(e.as_str()) {
                Some(id) => (*id).to_string(),
                None if exists(e) => e.clone(),
                None => return Err(format!("task: `after` names unknown job `{e}`")),
            };
            if id == ids[i] {
                return Err(format!("task: `{}` cannot wait for itself", specs[i].0));
            }
            if !deps.contains(&id) {
                deps.push(id);
            }
        }
        resolved.push(deps);
    }
    // A cycle can only run through this call's own tasks.
    let edges: Vec<Vec<usize>> =
        resolved.iter().map(|deps| deps.iter().filter_map(|d| ids.iter().position(|i| i == d)).collect()).collect();
    // 0 unvisited, 1 on the current path, 2 done.
    let mut mark = vec![0u8; tasks.len()];
    let mut path: Vec<usize> = Vec::new();
    for start in 0..tasks.len() {
        if let Some(cycle) = find_cycle(start, &edges, &mut mark, &mut path) {
            let names: Vec<&str> = cycle.iter().map(|&i| specs[i].0.as_str()).collect();
            return Err(format!("task: `after` forms a cycle: {}", names.join(" → ")));
        }
    }
    Ok(resolved)
}

fn find_cycle(at: usize, edges: &[Vec<usize>], mark: &mut [u8], path: &mut Vec<usize>) -> Option<Vec<usize>> {
    match mark[at] {
        1 => {
            let from = path.iter().position(|&p| p == at).unwrap_or(0);
            let mut cycle = path[from..].to_vec();
            cycle.push(at);
            return Some(cycle);
        }
        2 => return None,
        _ => {}
    }
    mark[at] = 1;
    path.push(at);
    for &next in &edges[at] {
        if let Some(c) = find_cycle(next, edges, mark, path) {
            return Some(c);
        }
    }
    path.pop();
    mark[at] = 2;
    None
}

/// What the started-jobs line says about a job that is not running yet.
fn queued_note(jobs: &crate::jobs::JobsHandle, id: &str, after: &[String]) -> String {
    use mu_base::events::SubagentState;
    match jobs.state_of(id) {
        Some(SubagentState::Queued) => {
            let waiting: Vec<&str> =
                after.iter().map(String::as_str).filter(|d| jobs.state_of(d) != Some(SubagentState::Done)).collect();
            if waiting.is_empty() {
                " [queued: no free slot]".to_string()
            } else {
                format!(" [queued: waits for {}]", waiting.join(", "))
            }
        }
        Some(SubagentState::Failed) => " [not started: a job it waited for failed]".to_string(),
        _ => String::new(),
    }
}

/// What starts a foreground task's child: this executable, or a worker on the remote for the agent
/// kinds `[task] launch` names, which needs a launch prepared per task.
struct Starter<'a> {
    exe: &'a Path,
    launch: Option<&'a WorkerLaunch>,
    mirror: &'a dyn Fn(&str) -> Mirror,
    /// Each task's time limit.
    timeouts: &'a [Option<Duration>],
}

/// Builds one child's task payload: the task's index, agent, prompt, output schema and model role.
type Payload<'a> = dyn Fn(usize, &str, &str, Option<&Value>, Option<&str>) -> Value + 'a;

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
    relay: Option<&RelayServer>,
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
                changes: None,
                progress: None,
                ports: Vec::new(),
            })
            .collect::<Vec<_>>(),
    );
    let futs = specs.iter().enumerate().map(|(i, (name, agent, task, cwd, model))| {
        let rows = &rows;
        let payload = payload(i, agent, task, schemas[i], model.as_deref());
        let label = format!("{name} ({agent})");
        async move {
            let child = match how.launch.filter(|l| l.runs(agent)) {
                None => ChildCmd::Local(how.exe.to_path_buf()),
                Some(on) => match prepare_launch(on, cwd, name).await {
                    Ok(l) => ChildCmd::Launched(l),
                    Err(e) => {
                        rows.borrow_mut()[i].state = SubagentState::Failed;
                        return Outcome::failed(e);
                    }
                },
            };
            let out = run_child_controlled(
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
                (how.mirror)(agent),
                relay,
                ChildControl { steer: None, timeout: how.timeouts[i] },
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
        let s = super::schema(&["x"], &[], false);
        assert_eq!(
            s["properties"]["tasks"]["items"]["properties"]["agent"]["enum"],
            serde_json::json!(["task", "scout", "x"])
        );
    }

    #[test]
    fn the_model_field_is_offered_only_with_roles() {
        let s = super::schema(&[], &["@w"], false);
        assert_eq!(s["properties"]["tasks"]["items"]["properties"]["model"]["enum"], serde_json::json!(["@w"]));
        assert!(super::schema(&[], &[], false)["properties"]["tasks"]["items"]["properties"].get("model").is_none());
    }

    #[test]
    fn after_and_resume_are_offered_only_with_jobs() {
        let props = |jobs| super::schema(&[], &[], jobs)["properties"]["tasks"]["items"]["properties"].clone();
        let (with, without) = (props(true), props(false));
        assert!(with.get("after").is_some() && with.get("resume").is_some());
        assert!(without.get("after").is_none() && without.get("resume").is_none());
        for p in [&with, &without] {
            assert!(p.get("timeout").is_some() && p.get("maxRequests").is_some() && p.get("maxCost").is_some());
        }
    }

    #[test]
    fn tasks_is_required_at_the_top_level() {
        let s = super::schema(&[], &[], false);
        assert_eq!(s["required"], serde_json::json!(["tasks"]));
        assert!(s["properties"]["tasks"].get("required").is_none());
    }
}

#[cfg(test)]
mod cwd_tests {
    use super::*;
    use crate::tools::subagent::{run_child, run_child_observed};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn schema_accepts_an_optional_cwd_per_task() {
        let s = schema(&[], &[], false);
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
                None,
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
        let s = schema(&[], &[], false);
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
        ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None }
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
    async fn the_payload_carries_mus_version() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-task-version-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, ECHO_CHILD).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut t = Tools::for_test();
        t.child_exe = Some(exe);
        let a: Args = serde_json::from_value(json!({"tasks":[{"task":"x","solutionSpace":"one fix: given"}]})).unwrap();
        let out = tokio::task::LocalSet::new().run_until(t.task(a, &ctx())).await;
        assert!(out.text.contains(&format!(r#""muVersion":"{MU_VERSION}""#)), "{}", out.text);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_relayed_parent_passes_its_own_models_on_unchanged() {
        let t = Tools::for_test();
        assert_eq!(t.relay_allow("task", None), RelayAllow::default(), "no providers, nothing to allow");
        t.plugins.set_relay_allow(Some(&json!({"ids": ["a/b"], "roles": ["task"]})));
        let given = RelayAllow { ids: vec!["a/b".into()], roles: vec!["task".into()] };
        for (agent, model) in [("task", None), ("scout", None), ("task", Some("@other"))] {
            assert_eq!(t.relay_allow(agent, model), given, "{agent} {model:?}: a grandchild cannot widen it");
        }
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
    async fn only_the_kinds_launch_names_go_through_the_launcher() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-task-route-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, ECHO_CHILD).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = mu_testutil::repo();
        let launcher = base.join("launcher.sh");
        let log = base.join("launched");
        std::fs::write(
            &launcher,
            format!(
                "IFS= read -r a\necho \"$MU_TASK_NAME\" >> {}\necho '{{\"type\":\"result\",\"text\":\"remote says hi\"}}'\n",
                log.display()
            ),
        )
        .unwrap();
        let mut t = Tools::for_test();
        t.child_exe = Some(exe);
        t.launch = Some(WorkerLaunch {
            launcher: vec!["sh".into(), launcher.to_string_lossy().into_owned()],
            kinds: vec!["task".into()],
        });
        let cwd = root.to_str().unwrap();
        let a: Args = serde_json::from_value(json!({"tasks":[
            {"name":"Local","task":"x","agent":"scout","solutionSpace":"one fix: given","cwd":cwd},
            {"name":"Far","task":"y","agent":"task","solutionSpace":"one fix: given","cwd":cwd}
        ]}))
        .unwrap();
        let out = tokio::task::LocalSet::new().run_until(t.task(a, &ctx())).await;
        let (local, far) = out.text.split_once("## Far").expect("both rows");
        assert!(local.contains(r#""agent":"scout""#) && !local.contains("mirrorSession"), "{local}");
        assert!(!local.contains(r#""relay""#), "a local child is not relayed: {local}");
        assert!(far.contains("remote says hi"), "{far}");
        let launched = std::fs::read_to_string(&log).unwrap();
        assert!(launched.lines().count() == 1 && launched.starts_with("far-"), "the task, not the scout: {launched}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_launch_prepare_is_that_rows_failure() {
        let mut t = Tools::for_test();
        t.launch = Some(WorkerLaunch { launcher: vec!["true".into()], kinds: vec!["task".into()] });
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
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
        let a: Args = serde_json::from_value(
            json!({"tasks":[{"task":"x","solutionSpace":"one fix: given","outputSchema":{"type":"bogus"}}]}),
        )
        .unwrap();
        let out = t.task(a, &ctx).await;
        assert!(out.is_error && out.text.contains("outputSchema"), "{}", out.text);
    }
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;
    use crate::jobs::JobsHandle;

    fn ctx() -> ToolCtx {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None }
    }

    fn args(tasks: Value) -> Args {
        let tasks: Vec<Value> = tasks
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let mut t = t.clone();
                t["solutionSpace"] = json!("one fix: given");
                t
            })
            .collect();
        serde_json::from_value(json!({ "tasks": tasks })).unwrap()
    }

    fn resolve(tasks: Value, existing: &[&str]) -> Result<Vec<Vec<String>>, String> {
        let a = args(tasks);
        let specs: Vec<(String, String, String, PathBuf, Option<String>)> = a
            .tasks
            .iter()
            .map(|t| (t.name.clone().unwrap(), "scout".into(), t.task.clone(), PathBuf::new(), None))
            .collect();
        let ids: Vec<String> = specs.iter().map(|s| s.0.clone()).collect();
        resolve_after(&a.tasks, &specs, &ids, |e| existing.contains(&e))
    }

    #[test]
    fn after_names_tasks_of_this_call_or_existing_jobs() {
        let r = resolve(json!([{"name":"a","task":"x"},{"name":"b","task":"y","after":["a","old","a"]}]), &["old"])
            .unwrap();
        assert_eq!(r, [Vec::<String>::new(), vec!["a".to_string(), "old".to_string()]]);
    }

    #[test]
    fn after_rejects_unknown_self_and_cyclic_waits() {
        let e = resolve(json!([{"name":"a","task":"x","after":["zz"]}]), &[]).unwrap_err();
        assert_eq!(e, "task: `after` names unknown job `zz`");
        let e = resolve(json!([{"name":"a","task":"x","after":["a"]}]), &[]).unwrap_err();
        assert_eq!(e, "task: `a` cannot wait for itself");
        let e = resolve(json!([{"name":"a","task":"x","after":["b"]},{"name":"b","task":"y","after":["a"]}]), &[])
            .unwrap_err();
        assert_eq!(e, "task: `after` forms a cycle: a → b → a");
        let ok = resolve(
            json!([{"name":"a","task":"x"},{"name":"b","task":"y","after":["a"]},{"name":"c","task":"z","after":["a","b"]}]),
            &[],
        );
        assert!(ok.is_ok(), "a diamond is not a cycle: {ok:?}");
    }

    #[tokio::test]
    async fn limits_and_background_only_fields_are_checked() {
        let t = Tools::for_test();
        for (field, text) in [
            (json!({"timeout": 0}), "task: timeout must be 1-7200 seconds"),
            (json!({"timeout": 7201}), "task: timeout must be 1-7200 seconds"),
            (json!({"maxRequests": 0}), "task: maxRequests must be at least 1"),
            (json!({"maxCost": 0.0}), "task: maxCost must be above 0"),
            (json!({"after": ["x"]}), "task: `after` and `resume` need an interactive session"),
            (json!({"resume": "x"}), "task: `after` and `resume` need an interactive session"),
        ] {
            let mut task = json!({"task": "x"});
            task.as_object_mut().unwrap().extend(field.as_object().unwrap().clone());
            let out = t.task(args(json!([task])), &ctx()).await;
            assert!(out.is_error && out.text == text, "{}", out.text);
        }
    }

    fn with_jobs() -> (Tools, Rc<JobsHandle>) {
        let mut t = Tools::for_test();
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, _erx) = tokio::sync::mpsc::unbounded_channel();
        let jobs = JobsHandle::new(wake, events);
        t.jobs = Some(jobs.clone());
        (t, jobs)
    }

    #[tokio::test]
    async fn resume_needs_a_finished_local_subagent_and_after_a_known_job() {
        let (t, jobs) = with_jobs();
        let out = t.task(args(json!([{"task": "x", "resume": "nope"}])), &ctx()).await;
        assert!(out.is_error && out.text == "task: resume: no job `nope`", "{}", out.text);
        jobs.push_running("run");
        let out = t.task(args(json!([{"task": "x", "resume": "run"}])), &ctx()).await;
        assert!(out.is_error && out.text.contains("is still running"), "{}", out.text);
        let out = t.task(args(json!([{"task": "x", "after": ["ghost"]}])), &ctx()).await;
        assert!(out.is_error && out.text == "task: `after` names unknown job `ghost`", "{}", out.text);
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
            None,
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
            let _ = tx.send(ApprovalDecision::Allow {
                scope: ApprovalScope::Once,
                note: None,
                patterns: None,
                model: None,
            });
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
