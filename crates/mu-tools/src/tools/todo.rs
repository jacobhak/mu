use super::Tools;
use mu_base::tool::ToolOutput;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Abandoned,
    Blocked,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Task {
    pub content: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Phase {
    pub name: String,
    pub tasks: Vec<Task>,
}

#[derive(Deserialize, Default)]
pub struct Args {
    op: Option<String>,
    list: Option<Vec<InitEntry>>,
    task: Option<String>,
    phase: Option<String>,
    items: Option<Vec<String>>,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct InitEntry {
    phase: String,
    items: Vec<String>,
}

const OPS: [&str; 9] = ["init", "start", "done", "rm", "drop", "block", "unblock", "append", "view"];

pub fn description() -> &'static str {
    include_str!("../prompts/todo-tool.md")
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "op":{"type":"string","enum":OPS},
        "list":{"type":"array","description":"phases for init","items":{"type":"object","properties":{
            "phase":{"type":"string"},"items":{"type":"array","items":{"type":"string"},"minItems":1}},"required":["phase","items"]}},
        "task":{"type":"string","description":"verbatim task content"},
        "phase":{"type":"string"},
        "items":{"type":"array","items":{"type":"string"},"description":"tasks for flat init or append"},
        "reason":{"type":"string","description":"blocker note for block"}},"required":["op"]})
}

fn find_task(phases: &[Phase], content: &str) -> Option<(usize, usize)> {
    phases.iter().enumerate().find_map(|(p, ph)| ph.tasks.iter().position(|t| t.content == content).map(|t| (p, t)))
}

fn resolve_task(phases: &[Phase], content: Option<&str>, errors: &mut Vec<String>) -> Option<(usize, usize)> {
    let Some(c) = content.filter(|c| !c.is_empty()) else {
        errors.push("Missing task content".into());
        return None;
    };
    let hit = find_task(phases, c);
    if hit.is_none() {
        if c.strip_prefix("task-").is_some_and(|n| !n.is_empty() && n.chars().all(|d| d.is_ascii_digit())) {
            errors.push(format!("Task \"{c}\" not found. Tasks are referenced by content, not by IDs — pass the task's full text from the previous result."));
        } else {
            let hint = if phases.iter().all(|p| p.tasks.is_empty()) {
                " (todo list is empty — was it replaced or not yet created?)"
            } else {
                ""
            };
            errors.push(format!("Task \"{c}\" not found{hint}"));
        }
    }
    hit
}

fn resolve_phase(phases: &[Phase], name: Option<&str>, errors: &mut Vec<String>) -> Option<usize> {
    let Some(n) = name.filter(|n| !n.is_empty()) else {
        errors.push("Missing phase name".into());
        return None;
    };
    let hit = phases.iter().position(|p| p.name == n);
    if hit.is_none() {
        errors.push(format!("Phase \"{n}\" not found"));
    }
    hit
}

/// (phase index, task index) targets for task / phase / everything.
fn targets(phases: &[Phase], a: &Args, errors: &mut Vec<String>) -> Vec<(usize, usize)> {
    if a.task.is_some() {
        return resolve_task(phases, a.task.as_deref(), errors).into_iter().collect();
    }
    if a.phase.is_some() {
        return resolve_phase(phases, a.phase.as_deref(), errors)
            .map(|p| (0..phases[p].tasks.len()).map(|t| (p, t)).collect())
            .unwrap_or_default();
    }
    phases.iter().enumerate().flat_map(|(p, ph)| (0..ph.tasks.len()).map(move |t| (p, t))).collect()
}

fn normalize(phases: &mut [Phase]) {
    let mut seen = false;
    for t in phases.iter_mut().flat_map(|p| p.tasks.iter_mut()) {
        if t.status == Status::InProgress {
            if seen {
                t.status = Status::Pending;
            }
            seen = true;
        }
    }
    if !seen && let Some(t) = phases.iter_mut().flat_map(|p| p.tasks.iter_mut()).find(|t| t.status == Status::Pending) {
        t.status = Status::InProgress;
    }
}

fn apply(mut phases: Vec<Phase>, op: &str, a: &Args, errors: &mut Vec<String>) -> Vec<Phase> {
    match op {
        "init" => {
            let list: Vec<(String, Vec<String>)> = match (&a.list, &a.items) {
                (Some(l), _) => l.iter().map(|e| (e.phase.clone(), e.items.clone())).collect(),
                (None, Some(i)) if !i.is_empty() => {
                    vec![(a.phase.clone().unwrap_or_else(|| "Tasks".into()), i.clone())]
                }
                _ => {
                    errors.push("Missing list for init operation".into());
                    return phases;
                }
            };
            let (mut sp, mut st) = (std::collections::HashSet::new(), std::collections::HashSet::new());
            for (p, items) in &list {
                if !sp.insert(p.clone()) {
                    errors.push(format!("Duplicate phase \"{p}\" in init list"));
                }
                for i in items {
                    if !st.insert(i.clone()) {
                        errors.push(format!("Duplicate task \"{i}\" in init list"));
                    }
                }
            }
            list.into_iter()
                .map(|(name, items)| Phase {
                    name,
                    tasks: items
                        .into_iter()
                        .map(|content| Task { content, status: Status::Pending, blocker: None })
                        .collect(),
                })
                .collect()
        }
        "start" => {
            if let Some((p, t)) = resolve_task(&phases, a.task.as_deref(), errors) {
                for (pi, ph) in phases.iter_mut().enumerate() {
                    for (ti, task) in ph.tasks.iter_mut().enumerate() {
                        if task.status == Status::InProgress && (pi, ti) != (p, t) {
                            task.status = Status::Pending;
                        }
                    }
                }
                phases[p].tasks[t].status = Status::InProgress;
            }
            phases
        }
        "done" | "drop" => {
            let status = if op == "done" { Status::Completed } else { Status::Abandoned };
            for (p, t) in targets(&phases, a, errors) {
                phases[p].tasks[t].status = status;
            }
            phases
        }
        "block" => {
            if a.task.is_none() && a.phase.is_none() {
                errors.push("block requires a task or phase target".into());
                return phases;
            }
            let reason = a
                .reason
                .as_deref()
                .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|r| !r.is_empty());
            for (p, t) in targets(&phases, a, errors) {
                let task = &mut phases[p].tasks[t];
                if matches!(task.status, Status::Pending | Status::InProgress | Status::Blocked) {
                    task.status = Status::Blocked;
                    task.blocker = reason.clone();
                }
            }
            phases
        }
        "unblock" => {
            if a.task.is_none() && a.phase.is_none() {
                errors.push("unblock requires a task or phase target".into());
                return phases;
            }
            for (p, t) in targets(&phases, a, errors) {
                let task = &mut phases[p].tasks[t];
                if task.status == Status::Blocked {
                    task.status = Status::Pending;
                    task.blocker = None;
                }
            }
            phases
        }
        "rm" => {
            if let Some(c) = &a.task {
                if let Some((p, t)) = resolve_task(&phases, Some(c), errors) {
                    phases[p].tasks.remove(t);
                }
            } else if a.phase.is_some() {
                if let Some(p) = resolve_phase(&phases, a.phase.as_deref(), errors) {
                    phases[p].tasks.clear();
                }
            } else {
                phases.iter_mut().for_each(|p| p.tasks.clear());
            }
            phases
        }
        "append" => {
            let Some(phase) = a.phase.as_deref().filter(|p| !p.is_empty()) else {
                errors.push("Missing phase name for append operation".into());
                return phases;
            };
            let items = a.items.as_deref().unwrap_or_default();
            if items.is_empty() {
                errors.push("Missing items for append operation".into());
                return phases;
            }
            let mut seen = std::collections::HashSet::new();
            let mut dup = false;
            for i in items {
                if !seen.insert(i.as_str()) || find_task(&phases, i).is_some() {
                    errors.push(format!("Task \"{i}\" already exists"));
                    dup = true;
                }
            }
            if dup {
                return phases;
            }
            let idx = match phases.iter().position(|p| p.name == phase) {
                Some(i) => i,
                None => {
                    phases.push(Phase { name: phase.into(), tasks: vec![] });
                    phases.len() - 1
                }
            };
            phases[idx].tasks.extend(items.iter().map(|c| Task {
                content: c.clone(),
                status: Status::Pending,
                blocker: None,
            }));
            phases
        }
        _ => phases,
    }
}

fn infer_op(a: &Args, has_phases: bool) -> Option<&'static str> {
    if a.list.as_ref().is_some_and(|l| !l.is_empty()) {
        return Some("init");
    }
    if a.items.as_ref().is_some_and(|i| !i.is_empty()) {
        if a.phase.as_deref().is_some_and(|p| !p.is_empty()) {
            return Some("append");
        }
        if !has_phases {
            return Some("init");
        }
    }
    None
}

fn tag(t: &Task) -> String {
    match t.status {
        Status::InProgress => " (in progress)".into(),
        Status::Abandoned => " (dropped)".into(),
        Status::Blocked => t.blocker.as_ref().map_or(" (blocked)".into(), |b| format!(" (blocked: {b})")),
        _ => String::new(),
    }
}

fn open(t: &Task) -> bool {
    matches!(t.status, Status::Pending | Status::InProgress)
}

fn closed(t: &Task) -> bool {
    matches!(t.status, Status::Completed | Status::Abandoned)
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Pending => "pending",
        Status::InProgress => "in_progress",
        Status::Completed => "completed",
        Status::Abandoned => "abandoned",
        Status::Blocked => "blocked",
    }
}

pub fn format_summary(phases: &[Phase], errors: &[String], read_only: bool) -> String {
    let all: Vec<&Task> = phases.iter().flat_map(|p| p.tasks.iter()).collect();
    if all.is_empty() {
        if !errors.is_empty() {
            return format!("Errors: {}", errors.join("; "));
        }
        return if read_only { "Todo list is empty.".into() } else { "Todo list cleared.".into() };
    }
    let remaining: Vec<(&Task, &str)> =
        phases.iter().flat_map(|p| p.tasks.iter().filter(|t| open(t)).map(move |t| (t, p.name.as_str()))).collect();
    let cur_idx = phases.iter().position(|p| p.tasks.iter().any(open)).unwrap_or(phases.len() - 1);
    let cur = &phases[cur_idx];
    let done = cur.tasks.iter().filter(|t| closed(t)).count();
    let mut lines: Vec<String> = Vec::new();
    if !errors.is_empty() {
        lines.push(format!("Errors: {}", errors.join("; ")));
    }
    if remaining.is_empty() {
        lines.push("Remaining items: none.".into());
    } else {
        lines.push(format!("Remaining items ({}):", remaining.len()));
        for (t, p) in &remaining {
            lines.push(format!("  - {} [{}] ({p})", t.content, status_name(t.status)));
        }
    }
    let closed_all = all.iter().filter(|t| closed(t)).count();
    let blocked = all.iter().filter(|t| t.status == Status::Blocked).count();
    let ahead = phases.iter().enumerate().any(|(i, p)| i > cur_idx && p.tasks.iter().any(closed));
    lines.push(format!(
        "Overall: {closed_all}/{} done, {} open{}.",
        all.len(),
        remaining.len(),
        if blocked > 0 { format!(", {blocked} blocked") } else { String::new() }
    ));
    lines.push(format!(
        "Active phase {}/{} \"{}\" ({done}/{}){}",
        cur_idx + 1,
        phases.len(),
        cur.name,
        cur.tasks.len(),
        if ahead {
            " — earliest phase with open tasks; the in-progress pointer auto-advances to the earliest open task on each completion, so it can sit behind out-of-order work (nothing was un-completed)."
        } else {
            "."
        }
    ));
    for p in phases {
        lines.push(format!("  {}:", p.name));
        for t in &p.tasks {
            lines.push(format!(
                "    - {} {}{}",
                if t.status == Status::Completed { "[X]" } else { "[ ]" },
                t.content,
                tag(t)
            ));
        }
    }
    lines.join("\n")
}

impl Tools {
    pub fn set_todo(&self, phases: Vec<Phase>) {
        *self.todo.borrow_mut() = phases;
    }

    pub fn todo_json(&self) -> Value {
        serde_json::to_value(&*self.todo.borrow()).unwrap_or(Value::Null)
    }

    /// Contents of tasks still pending or in progress (blocked and closed ones excluded).
    pub fn todo_open(&self) -> Vec<String> {
        self.todo.borrow().iter().flat_map(|p| p.tasks.iter()).filter(|t| open(t)).map(|t| t.content.clone()).collect()
    }

    pub fn todo(&self, mut a: Args) -> ToolOutput {
        let previous = self.todo.borrow().clone();
        let op = match a.op.take() {
            Some(op) if OPS.contains(&op.as_str()) => op,
            Some(op) => return todo_err(&previous, format!("Invalid todo arguments: unknown op `{op}`")),
            None => match infer_op(&a, !previous.is_empty()) {
                Some(op) => op.into(),
                None => return todo_err(&previous, "Invalid todo arguments: `op` is required".into()),
            },
        };
        let read_only = op == "view";
        let mut errors = Vec::new();
        let updated = if read_only {
            previous.clone()
        } else {
            let mut u = apply(previous.clone(), &op, &a, &mut errors);
            normalize(&mut u);
            u
        };
        let effective = if errors.is_empty() { updated } else { previous };
        if errors.is_empty() && !read_only {
            self.set_todo(effective.clone());
        }
        let mut out = ToolOutput::ok(format_summary(&effective, &errors, read_only));
        out.is_error = !errors.is_empty();
        out.details = Some(json!({"op": op, "phases": effective}));
        out
    }
}

fn todo_err(phases: &[Phase], msg: String) -> ToolOutput {
    let mut o = ToolOutput::err(msg);
    o.details = Some(json!({"op": "view", "phases": phases}));
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &Tools, v: Value) -> ToolOutput {
        t.todo(serde_json::from_value(v).unwrap())
    }

    #[test]
    fn lifecycle_promotion_and_atomic_errors() {
        let t = Tools::for_test();
        let o = run(&t, json!({"op":"init","list":[{"phase":"P","items":["a","b"]}]}));
        assert!(o.text.contains("a [in_progress] (P)"), "{}", o.text);
        let o = run(&t, json!({"op":"done","task":"a"}));
        assert!(o.text.contains("- [ ] b (in progress)") && o.text.contains("Overall: 1/2 done"), "{}", o.text);
        let before = t.todo_json();
        let o = run(&t, json!({"op":"append","phase":"P","items":["c","a"]}));
        assert!(o.is_error && o.text.contains("Task \"a\" already exists"));
        assert_eq!(t.todo_json(), before);
        let o = run(&t, json!({"op":"done","task":"task-1"}));
        assert!(o.is_error && o.text.contains("referenced by content"));
        let o = run(&t, json!({"items":["x"],"phase":"Q"}));
        assert!(!o.is_error, "{}", o.text);
        let o = run(&t, json!({"op":"block","task":"b","reason":"waiting\n on  user"}));
        assert!(o.text.contains("(blocked: waiting on user)"), "{}", o.text);
    }
}
