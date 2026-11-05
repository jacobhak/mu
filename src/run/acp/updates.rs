//! Agent events as ACP `session/update` payloads, and the state that spans events: output that
//! streams while a tool runs, and background jobs shown as tool calls.

use mu_base::events::{AgentEvent, JobKind, SubagentState, SubagentStatus};
use mu_base::text::truncate_text;
use mu_base::tool::ToolMeta;
use mu_tools::tools::summary;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Newest bytes of a running call's output sent to the client.
const KEEP: usize = 16 * 1024;
/// Least time between two updates of one running call.
const EVERY: Duration = Duration::from_millis(250);

pub(super) fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" => "read",
        "grep" | "glob" => "search",
        "edit" | "write" => "edit",
        "bash" => "execute",
        "fetch" | "web_search" => "fetch",
        "propose_plan" => "switch_mode",
        _ => "other",
    }
}

/// The files a call names, absolute: `path` for `read` and `write`, the `[path#tag]` headers of an
/// `edit` input. Other schemes (`local://`) and tools name none.
pub(super) fn tool_paths(cwd: &Path, name: &str, args: &Value) -> Vec<PathBuf> {
    let mut raw: Vec<&str> = Vec::new();
    match name {
        "read" | "write" => raw.extend(args["path"].as_str()),
        "edit" => {
            for line in args["input"].as_str().unwrap_or("").lines() {
                let Some(head) = line.strip_prefix('[').and_then(|l| l.split_once(']')).map(|(h, _)| h) else {
                    continue;
                };
                if let Some((path, _)) = head.split_once('#') {
                    raw.push(path);
                }
            }
        }
        _ => {}
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for p in raw.into_iter().filter(|p| !p.is_empty() && !p.contains("://")) {
        let abs = cwd.join(p);
        if !out.contains(&abs) {
            out.push(abs);
        }
    }
    out
}

fn text_block(t: impl Into<String>) -> Value {
    json!([{"type": "content", "content": {"type": "text", "text": t.into()}}])
}

pub(super) fn plan_entries(phases: &Value) -> Vec<Value> {
    let phases = phases.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut out = Vec::new();
    for ph in phases {
        for t in ph["tasks"].as_array().into_iter().flatten() {
            let mut content = t["content"].as_str().unwrap_or("").to_string();
            if phases.len() > 1 {
                content = format!("{}: {content}", ph["name"].as_str().unwrap_or(""));
            }
            let status = match t["status"].as_str() {
                Some("in_progress") => "in_progress",
                Some("completed") => "completed",
                Some("abandoned") => {
                    content.push_str(" (abandoned)");
                    "completed"
                }
                Some("blocked") => {
                    match t["blocker"].as_str() {
                        Some(b) => content.push_str(&format!(" (blocked: {b})")),
                        None => content.push_str(" (blocked)"),
                    }
                    "pending"
                }
                _ => "pending",
            };
            out.push(json!({"content": content, "priority": "medium", "status": status}));
        }
    }
    out
}

/// ACP `session/update` payloads for one agent event. `live` is false while replaying history.
pub(super) fn translate(cwd: &Path, e: AgentEvent, live: bool) -> Vec<Value> {
    let text = |kind: &str, t: String| json!({"sessionUpdate": kind, "content": {"type": "text", "text": t}});
    match e {
        AgentEvent::UserPrompt(t) if !live => vec![text("user_message_chunk", t)],
        AgentEvent::Text(t) => vec![text("agent_message_chunk", t)],
        AgentEvent::Thinking(t) => vec![text("agent_thought_chunk", t)],
        AgentEvent::ToolStart { id, name, args } => {
            let paths = tool_paths(cwd, &name, &args);
            let shown = |p: &PathBuf| p.strip_prefix(cwd).unwrap_or(p).display().to_string();
            let title = match (name.as_str(), paths.is_empty()) {
                ("edit", false) => format!("edit: {}", paths.iter().map(shown).collect::<Vec<_>>().join(", ")),
                _ => match summary(&name, &args) {
                    s if s.is_empty() => name.clone(),
                    s => format!("{name}: {s}"),
                },
            };
            let mut call = json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "name": name,
                "title": title,
                "kind": tool_kind(&name),
                "status": "in_progress",
                "rawInput": args,
            });
            if !paths.is_empty() {
                call["locations"] = json!(paths.iter().map(|p| json!({"path": p})).collect::<Vec<_>>());
            }
            vec![call]
        }
        AgentEvent::ToolEnd { id, output, .. } => {
            let mut u = json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": if output.is_error { "failed" } else { "completed" }});
            let mut content: Vec<Value> = output
                .changes
                .iter()
                .map(|c| {
                    json!({"type": "diff", "path": c.path, "oldText": c.old, "newText": c.new.clone().unwrap_or_default()})
                })
                .collect();
            if !output.text.is_empty() {
                content.extend(text_block(output.text.clone()).as_array().cloned().unwrap_or_default());
            }
            if !content.is_empty() {
                u["content"] = Value::Array(content);
            }
            u["rawOutput"] = match &output.meta {
                Some(ToolMeta::Bash(m)) => {
                    // `output_end` cuts the command's output from the notes mu adds after it; a
                    // clean exit records no code.
                    let end = m.output_end.min(output.text.len());
                    let body = if output.text.is_char_boundary(end) { &output.text[..end] } else { &output.text };
                    json!({"output": body, "exitCode": m.exit_code.or((!m.failed()).then_some(0))})
                }
                _ => json!({"text": output.text}),
            };
            vec![u]
        }
        AgentEvent::Todo(v) => {
            let entries = plan_entries(&v);
            if entries.is_empty() && !live {
                vec![]
            } else {
                vec![json!({"sessionUpdate": "plan", "entries": entries})]
            }
        }
        _ => vec![],
    }
}

/// Output of running calls: the newest part of it goes to the client at most every `EVERY`, and
/// whatever is left over when the burst ends is sent by `due`.
#[derive(Default)]
pub(super) struct Stream {
    calls: HashMap<String, Live>,
}

struct Live {
    text: String,
    sent: Option<Instant>,
    dirty: bool,
}

fn running(id: &str, text: &str, cwd: &Path) -> Value {
    let from = {
        let mut f = text.len().saturating_sub(KEEP);
        while !text.is_char_boundary(f) {
            f += 1;
        }
        f
    };
    let tail = mu_base::secrets::redact(&text[from..], cwd).text;
    json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "in_progress", "content": text_block(tail)})
}

impl Stream {
    /// Adds output to call `id`. Returns the update to send now, when one is due.
    pub fn append(&mut self, id: &str, chunk: &str, cwd: &Path) -> Option<Value> {
        let live = self.calls.entry(id.into()).or_insert(Live { text: String::new(), sent: None, dirty: false });
        live.text.push_str(chunk);
        if live.text.len() > 2 * KEEP {
            let mut cut = live.text.len() - KEEP;
            while !live.text.is_char_boundary(cut) {
                cut += 1;
            }
            live.text.drain(..cut);
        }
        live.dirty = true;
        Self::send_if_due(id, live, Instant::now(), cwd)
    }

    /// Sets call `id`'s whole text (a job's latest activity).
    pub fn replace(&mut self, id: &str, text: String, cwd: &Path) -> Option<Value> {
        let live = self.calls.entry(id.into()).or_insert(Live { text: String::new(), sent: None, dirty: false });
        if live.text == text {
            return None;
        }
        live.text = text;
        live.dirty = true;
        Self::send_if_due(id, live, Instant::now(), cwd)
    }

    fn send_if_due(id: &str, live: &mut Live, now: Instant, cwd: &Path) -> Option<Value> {
        if !live.dirty || live.sent.is_some_and(|t| now.duration_since(t) < EVERY) {
            return None;
        }
        live.dirty = false;
        live.sent = Some(now);
        Some(running(id, &live.text, cwd))
    }

    /// Updates that were held back and are due at `now`.
    pub fn due(&mut self, now: Instant, cwd: &Path) -> Vec<Value> {
        let mut ids: Vec<&String> = self.calls.keys().collect();
        ids.sort();
        let ids: Vec<String> = ids.into_iter().cloned().collect();
        ids.iter().filter_map(|id| self.calls.get_mut(id).and_then(|l| Self::send_if_due(id, l, now, cwd))).collect()
    }

    /// When the next held-back update falls due.
    pub fn next_due(&self) -> Option<Instant> {
        self.calls.values().filter(|l| l.dirty).filter_map(|l| l.sent.map(|t| t + EVERY)).min()
    }

    /// The call is over: its final update replaces whatever was streamed.
    pub fn end(&mut self, id: &str) {
        self.calls.remove(id);
    }

    pub fn clear(&mut self) {
        self.calls.clear();
    }
}

/// What the client was last told about a job.
struct Seen {
    state: SubagentState,
    /// A bash job's newest output.
    tail: String,
}

/// Background jobs shown as tool calls: one per job, `in_progress` with its latest activity until
/// it ends.
#[derive(Default)]
pub(super) struct Jobs {
    seen: HashMap<String, Seen>,
}

fn job_id(name: &str) -> String {
    format!("job:{name}")
}

fn status_of(s: &SubagentState) -> &'static str {
    match s {
        SubagentState::Running => "in_progress",
        SubagentState::Done => "completed",
        SubagentState::Failed => "failed",
    }
}

fn job_text(j: &SubagentStatus) -> String {
    match j.kind {
        JobKind::Subagent => format!("{} tools · {}", j.tools, j.last).trim_end_matches(" · ").to_string(),
        JobKind::Bash => j.note.clone().unwrap_or_default(),
    }
}

impl Jobs {
    /// Updates for a snapshot of the jobs; running text goes through `stream` so it is throttled.
    pub fn update(&mut self, list: &[SubagentStatus], stream: &mut Stream, cwd: &Path) -> Vec<Value> {
        let mut out = Vec::new();
        for j in list {
            let id = job_id(&j.name);
            let prev = self.seen.get(&j.name).map(|s| s.state.clone());
            if prev.is_none() {
                let title = match j.kind {
                    JobKind::Subagent => format!("subagent {} ({})", j.name, j.agent),
                    JobKind::Bash => format!("bash job {}: {}", j.name, truncate_text(&j.task, 80)),
                };
                let name = if j.kind == JobKind::Bash { "bash" } else { "task" };
                out.push(json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": id,
                    "name": name,
                    "title": title,
                    "kind": if j.kind == JobKind::Bash { "execute" } else { "other" },
                    "status": status_of(&j.state),
                    "rawInput": {"agent": j.agent, "task": j.task, "cwd": j.cwd},
                }));
            }
            match j.state {
                SubagentState::Running => {
                    if j.kind == JobKind::Subagent {
                        out.extend(stream.replace(&id, job_text(j), cwd));
                    }
                }
                _ if prev.as_ref() != Some(&j.state) => {
                    stream.end(&id);
                    let mut u =
                        json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status_of(&j.state)});
                    let mut text = job_text(j);
                    if let Some(tail) = self.seen.get(&j.name).map(|s| s.tail.trim_end()).filter(|t| !t.is_empty()) {
                        text = if text.is_empty() { tail.to_string() } else { format!("{tail}\n{text}") };
                    }
                    if !text.is_empty() {
                        u["content"] = text_block(text);
                    }
                    out.push(u);
                }
                _ => {}
            }
            let tail = self.seen.get(&j.name).map(|s| s.tail.clone()).unwrap_or_default();
            self.seen.insert(j.name.clone(), Seen { state: j.state.clone(), tail });
        }
        out
    }

    /// The newest output of a running `bash` job. Output that arrives after the job ended is dropped.
    pub fn output(&mut self, name: &str, tail: String, stream: &mut Stream, cwd: &Path) -> Option<Value> {
        let seen = self.seen.get_mut(name).filter(|s| s.state == SubagentState::Running)?;
        seen.tail = tail.clone();
        stream.replace(&job_id(name), tail, cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::tool::{FileChange, ToolOutput};
    use std::time::SystemTime;

    fn kinds(v: &[Value]) -> Vec<&str> {
        v.iter().map(|u| u["sessionUpdate"].as_str().unwrap()).collect()
    }

    fn job(name: &str, kind: JobKind, state: SubagentState) -> SubagentStatus {
        SubagentStatus {
            name: name.into(),
            agent: if kind == JobKind::Bash { "bash".into() } else { "scout".into() },
            tools: 2,
            last: "read a.rs".into(),
            state,
            kind,
            cwd: PathBuf::from("/w"),
            task: "find it".into(),
            started: SystemTime::now(),
            ended: None,
            delivered: false,
            note: None,
            session: None,
            model: None,
        }
    }

    #[test]
    fn history_replays_user_text_but_live_prompts_are_not_echoed() {
        let cwd = Path::new("/w");
        assert_eq!(kinds(&translate(cwd, AgentEvent::UserPrompt("hi".into()), false)), ["user_message_chunk"]);
        assert!(translate(cwd, AgentEvent::UserPrompt("hi".into()), true).is_empty());
    }

    #[test]
    fn tool_call_carries_name_kind_title_and_absolute_location() {
        let u = translate(
            Path::new("/w"),
            AgentEvent::ToolStart { id: "c1".into(), name: "read".into(), args: json!({"path": "a/b.rs"}) },
            true,
        );
        assert_eq!((u[0]["kind"].as_str(), u[0]["name"].as_str()), (Some("read"), Some("read")));
        assert_eq!(u[0]["title"], "read: a/b.rs");
        assert_eq!(u[0]["locations"][0]["path"], "/w/a/b.rs");
        let u = translate(
            Path::new("/w"),
            AgentEvent::ToolStart { id: "c2".into(), name: "bash".into(), args: json!({"command": "ls"}) },
            true,
        );
        assert_eq!((u[0]["kind"].as_str(), u[0].get("locations")), (Some("execute"), None));
    }

    #[test]
    fn an_edit_names_its_files_from_the_hashline_headers() {
        let input =
            "[src/a.rs#AB12]\nPUT 1.=1:\n+x\n[src/b.rs#CD34]\nPUT 2.=2:\n+y\n[local://plan.md#EF56]\nPUT 1.=1:\n+z\n";
        let u = translate(
            Path::new("/w"),
            AgentEvent::ToolStart { id: "e".into(), name: "edit".into(), args: json!({"input": input}) },
            true,
        );
        assert_eq!(u[0]["title"], "edit: src/a.rs, src/b.rs");
        assert_eq!(u[0]["kind"], "edit");
        let paths: Vec<&str> =
            u[0]["locations"].as_array().unwrap().iter().map(|l| l["path"].as_str().unwrap()).collect();
        assert_eq!(paths, ["/w/src/a.rs", "/w/src/b.rs"]);
        // No header it understands: the generic title, no locations.
        let u = translate(
            Path::new("/w"),
            AgentEvent::ToolStart { id: "e".into(), name: "edit".into(), args: json!({"input": "odd"}) },
            true,
        );
        assert_eq!((u[0]["title"].as_str(), u[0].get("locations")), (Some("edit: odd"), None));
    }

    #[test]
    fn a_finished_write_carries_a_diff_and_the_raw_output() {
        let mut out = ToolOutput::ok("wrote it");
        out.changes = vec![
            FileChange { path: "/w/a.rs".into(), old: Some("a\n".into()), new: Some("b\n".into()) },
            FileChange { path: "/w/new.rs".into(), old: None, new: Some("n".into()) },
        ];
        let u =
            translate(Path::new("/w"), AgentEvent::ToolEnd { id: "c".into(), name: "write".into(), output: out }, true);
        let c = u[0]["content"].as_array().unwrap();
        assert_eq!(
            (c[0]["type"].as_str(), c[0]["path"].as_str(), c[0]["oldText"].as_str()),
            (Some("diff"), Some("/w/a.rs"), Some("a\n"))
        );
        assert_eq!((c[1]["oldText"].is_null(), c[1]["newText"].as_str()), (true, Some("n")));
        assert_eq!(c[2]["content"]["text"], "wrote it");
        assert_eq!(u[0]["rawOutput"]["text"], "wrote it");
    }

    #[test]
    fn failed_tool_maps_to_failed_status() {
        let out = ToolOutput::err("boom");
        let u =
            translate(Path::new("/w"), AgentEvent::ToolEnd { id: "c1".into(), name: "bash".into(), output: out }, true);
        assert_eq!(
            (u[0]["status"].as_str(), u[0]["content"][0]["content"]["text"].as_str()),
            (Some("failed"), Some("boom"))
        );
    }

    #[test]
    fn todo_becomes_plan_and_empty_replay_is_skipped() {
        let phases = json!([
            {"name": "A", "tasks": [{"content": "x", "status": "in_progress"}, {"content": "y", "status": "blocked", "blocker": "dep"}]},
            {"name": "B", "tasks": [{"content": "z", "status": "abandoned"}]},
        ]);
        let u = translate(Path::new("/w"), AgentEvent::Todo(phases), true);
        let e = u[0]["entries"].as_array().unwrap();
        let got: Vec<(&str, &str)> =
            e.iter().map(|x| (x["content"].as_str().unwrap(), x["status"].as_str().unwrap())).collect();
        assert_eq!(
            got,
            [("A: x", "in_progress"), ("A: y (blocked: dep)", "pending"), ("B: z (abandoned)", "completed")]
        );
        assert!(translate(Path::new("/w"), AgentEvent::Todo(json!([])), false).is_empty());
        assert_eq!(translate(Path::new("/w"), AgentEvent::Todo(json!([])), true).len(), 1);
    }

    #[test]
    fn output_streams_at_most_every_interval_and_the_rest_waits_to_be_due() {
        let cwd = Path::new("/w");
        let mut s = Stream::default();
        let first = s.append("c", "one\n", cwd).expect("the first chunk goes at once");
        assert_eq!(first["status"], "in_progress");
        assert_eq!(first["content"][0]["content"]["text"], "one\n");
        assert!(s.append("c", "two\n", cwd).is_none(), "held back inside the interval");
        assert!(s.next_due().is_some());
        let later = Instant::now() + EVERY * 2;
        let due = s.due(later, cwd);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0]["content"][0]["content"]["text"], "one\ntwo\n", "the whole text, not the delta");
        assert!(s.due(later + EVERY * 2, cwd).is_empty() && s.next_due().is_none());
        s.end("c");
        assert!(s.calls.is_empty());
    }

    #[test]
    fn streamed_output_is_bounded_and_redacted() {
        let cwd = Path::new("/w");
        let mut s = Stream::default();
        let big = "x".repeat(3 * KEEP);
        let u = s.append("c", &big, cwd).unwrap();
        assert!(u["content"][0]["content"]["text"].as_str().unwrap().len() <= KEEP);
        let mut s = Stream::default();
        let key = format!("ghp_{}", "a1B2".repeat(9));
        let u = s.append("c", &format!("token {key}\n"), cwd).unwrap();
        assert!(!u["content"][0]["content"]["text"].as_str().unwrap().contains(&key));
    }

    #[test]
    fn a_job_is_a_tool_call_that_ends_with_its_state() {
        let cwd = Path::new("/w");
        let (mut jobs, mut stream) = (Jobs::default(), Stream::default());
        let u = jobs.update(&[job("scout-1", JobKind::Subagent, SubagentState::Running)], &mut stream, cwd);
        assert_eq!(kinds(&u), ["tool_call", "tool_call_update"]);
        assert_eq!((u[0]["toolCallId"].as_str(), u[0]["status"].as_str()), (Some("job:scout-1"), Some("in_progress")));
        assert_eq!(u[0]["title"], "subagent scout-1 (scout)");
        assert_eq!(u[1]["content"][0]["content"]["text"], "2 tools · read a.rs");
        // Unchanged: nothing to say.
        assert!(jobs.update(&[job("scout-1", JobKind::Subagent, SubagentState::Running)], &mut stream, cwd).is_empty());
        let u = jobs.update(&[job("scout-1", JobKind::Subagent, SubagentState::Done)], &mut stream, cwd);
        assert_eq!(kinds(&u), ["tool_call_update"]);
        assert_eq!(u[0]["status"], "completed");
        let u = jobs.update(&[job("scout-1", JobKind::Subagent, SubagentState::Done)], &mut stream, cwd);
        assert!(u.is_empty(), "said once");
    }

    #[test]
    fn a_bash_job_streams_its_output_and_ends_with_how_it_ended() {
        let cwd = Path::new("/w");
        let (mut jobs, mut stream) = (Jobs::default(), Stream::default());
        let u = jobs.update(&[job("bg_1", JobKind::Bash, SubagentState::Running)], &mut stream, cwd);
        assert_eq!((kinds(&u), u[0]["kind"].as_str()), (vec!["tool_call"], Some("execute")));
        let o = jobs.output("bg_1", "line\n".into(), &mut stream, cwd).unwrap();
        assert_eq!(o["content"][0]["content"]["text"], "line\n");
        assert!(jobs.output("nope", "x".into(), &mut stream, cwd).is_none());
        let mut done = job("bg_1", JobKind::Bash, SubagentState::Failed);
        done.note = Some("exit 3".into());
        let u = jobs.update(&[done], &mut stream, cwd);
        assert_eq!(
            (u[0]["status"].as_str(), u[0]["content"][0]["content"]["text"].as_str()),
            (Some("failed"), Some("line\nexit 3")),
            "the last output stays, with how it ended"
        );
        let late = jobs.output("bg_1", "late\n".into(), &mut stream, cwd);
        assert!(late.is_none(), "output after the end is dropped");
        assert!(stream.calls.is_empty());
    }
}
