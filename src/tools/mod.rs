mod ask;
mod bash;
mod diff;
mod edit;
mod glob;
pub mod grep;
mod plan;
pub mod task;
mod read;
pub mod todo;
pub mod uri;
mod wait;
mod write;

use crate::provider::ToolDef;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use crate::events::EventTx;
use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use crate::jobs::JobsHandle;
use crate::plugin::PluginHost;
use tokio_util::sync::CancellationToken;

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    /// UI-only rendering (e.g. a diff); never sent to the model.
    pub display: Option<String>,
    pub details: Option<Value>,
}

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false, display: None, details: None }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, display: None, details: None }
    }
}

pub struct ToolCtx {
    pub events: EventTx,
    pub cancel: CancellationToken,
    pub call_id: String,
}

pub const BUILTIN_NAMES: [&str; 11] = ["read", "write", "edit", "bash", "grep", "glob", "todo", "ask", "propose_plan", "task", "wait"];

pub struct Tools {
    pub cwd: PathBuf,
    pub plugins: Rc<PluginHost>,
    /// Set in interactive sessions: `task` then runs in the background.
    pub jobs: Option<Rc<JobsHandle>>,
    todo: std::cell::RefCell<Vec<todo::Phase>>,
    session_id: std::cell::RefCell<String>,
    session_path: std::cell::RefCell<Option<PathBuf>>,
    skills: std::cell::RefCell<Vec<(String, PathBuf)>>,
    pub plan: std::cell::Cell<bool>,
    pub edit_store: pi_edit::EditStore,
    shell: OnceCell<pi_shell::shell::Shell>,
}

impl Tools {
    #[cfg(test)]
    pub fn for_test() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self::new(PathBuf::from("/tmp"), PluginHost::new(vec![], vec![], tx, Default::default()))
    }

    pub fn new(cwd: PathBuf, plugins: Rc<PluginHost>) -> Self {
        Self { cwd, plugins, jobs: None, todo: Default::default(), session_id: Default::default(), session_path: Default::default(), skills: Default::default(), plan: Default::default(), edit_store: pi_edit::EditStore::new(), shell: OnceCell::new() }
    }

    pub fn shell(&self) -> &pi_shell::shell::Shell {
        self.shell.get_or_init(|| pi_shell::shell::Shell::new(None))
    }

    pub fn abs(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { self.cwd.join(p) }
    }

    pub fn display(&self, abs: &Path) -> String {
        match abs.strip_prefix(&self.cwd) {
            Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
            Ok(_) => ".".into(),
            Err(_) => abs.to_string_lossy().into_owned(),
        }
    }

    pub fn definitions() -> Vec<ToolDef> {
        let defs = vec![
            json!({"name":"read","description":READ_DESC,"input_schema":{"type":"object","properties":{
                "path":{"type":"string","description":"File or directory path, optionally with a selector suffix"}},"required":["path"]}}),
            json!({"name":"write","description":"Create a file or replace it entirely. Creates parent directories. Returns a `[path#TAG]` header usable by `edit`.","input_schema":{"type":"object","properties":{
                "path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}),
            json!({"name":"edit","description":pi_edit::description(pi_edit::EditMode::Hashline),"input_schema":{"type":"object","properties":{
                "input":{"type":"string","description":"The edit payload, in the format described above"}},"required":["input"]}}),
            json!({"name":"bash","description":"Run a shell command. The shell persists across calls (cwd and exported variables carry over). Output is truncated in the middle when large.","input_schema":{"type":"object","properties":{
                "command":{"type":"string"},
                "timeout":{"type":"number","description":"Seconds; default 300, max 3600, 0 disables"},
                "cwd":{"type":"string","description":"Working directory for this command"},
                "pty":{"type":"boolean","description":"Run in a terminal the user can type into (passwords, sudo, prompts, confirmations). Fresh login shell, not the persistent one. Use only when the command needs user input."}},"required":["command"]}}),
            json!({"name":"grep","description":"Regex search in file contents (respects .gitignore, skips hidden files). Returns `path:line:text` rows; max 20 per file, 200 total.","input_schema":{"type":"object","properties":{
                "pattern":{"type":"string"},"path":{"type":"string","description":"File or directory; default ."},
                "case":{"type":"boolean","description":"Case-sensitive; default true"}},"required":["pattern"]}}),
            json!({"name":"glob","description":"Find files by glob pattern (e.g. `**/*.rs`), newest first. Respects .gitignore, skips hidden files.","input_schema":{"type":"object","properties":{
                "pattern":{"type":"string"},"path":{"type":"string","description":"Directory; default ."}},"required":["pattern"]}}),
        ];
        defs.into_iter()
            .chain([
                json!({"name":"todo","description":todo::description(),"input_schema":todo::schema()}),
                json!({"name":"ask","description":ask::description(),"input_schema":ask::schema()}),
            ])
            .map(|d| ToolDef {
                name: d["name"].as_str().unwrap_or_default().into(),
                description: d["description"].as_str().unwrap_or_default().into(),
                schema: d["input_schema"].clone(),
            })
            .collect()
    }

    /// `async_jobs`: `task` returns job ids immediately instead of blocking.
    pub fn task_def(async_jobs: bool, agents: &[crate::context::AgentDef]) -> ToolDef {
        let mut description = String::from(if async_jobs { task::ASYNC_DESCRIPTION } else { task::DESCRIPTION });
        if !agents.is_empty() {
            let list: Vec<String> = agents.iter().map(|a| format!("`{}`: {}", a.name, a.description)).collect();
            description.push_str("\nCustom agents: ");
            description.push_str(&list.join("; "));
        }
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        ToolDef { name: "task".into(), description, schema: task::schema(&names) }
    }

    pub fn wait_def() -> ToolDef {
        ToolDef { name: "wait".into(), description: wait::DESCRIPTION.into(), schema: wait::schema() }
    }

    pub fn plan_def() -> ToolDef {
        ToolDef { name: "propose_plan".into(), description: plan::DESCRIPTION.into(), schema: plan::schema() }
    }

    pub async fn call(&self, name: &str, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        if let Some(raw) = args.get("__invalid_json") {
            return ToolOutput::err(format!("Invalid arguments for {name}: arguments were not valid JSON: {raw}"));
        }
        match name {
            "read" => with_args(name, args, |a| self.read(a)),
            "write" => with_args(name, args, |a| self.write(a)),
            "grep" => with_args(name, args, |a| self.grep(a)),
            "glob" => with_args(name, args, |a| self.glob(a)),
            "edit" => match parse::<edit::Args>(name, args) {
                Ok(a) => self.edit(a).await,
                Err(e) => e,
            },
            "task" => match parse::<task::Args>(name, args) {
                Ok(a) => self.task(a, ctx).await,
                Err(e) => e,
            },
            "propose_plan" => match parse::<plan::Args>(name, args) {
                Ok(a) => self.propose_plan(a, ctx).await,
                Err(e) => e,
            },
            "todo" => with_args(name, args, |a| self.todo(a)),
            "ask" => match parse::<ask::Args>(name, args) {
                Ok(a) => self.ask(a, ctx).await,
                Err(e) => e,
            },
            "wait" => match parse::<wait::Args>(name, args) {
                Ok(a) => self.wait(a, ctx).await,
                Err(e) => e,
            },
            "bash" => match parse::<bash::Args>(name, args) {
                Ok(a) => self.bash(a, ctx).await,
                Err(e) => e,
            },
            _ if self.plugins.has_tool(name) => {
                let r = self.plugins.call_tool(name, args, &ctx.cancel).await;
                ToolOutput { text: r.content, is_error: r.is_error, display: r.display, details: None }
            }
            _ => ToolOutput::err(format!("Unknown tool: {name}")),
        }
    }
}

const READ_DESC: &str = "Read a file or list a directory. Text files are returned as `N:TEXT` rows under a `[path#TAG]` header; the header and line numbers are what `edit` anchors on. Default: first 300 lines. Selectors: `path:N` from line N, `path:N-M` range (with 1 line before, 3 after), `path:N+K` K lines from N, `path:-K` last K lines. Long lines are cut at 768 chars.";

fn parse<A: DeserializeOwned>(name: &str, args: &Value) -> Result<A, ToolOutput> {
    serde_json::from_value(args.clone())
        .map_err(|e| ToolOutput::err(format!("Invalid arguments for {name}: {e}")))
}

fn with_args<A: DeserializeOwned>(name: &str, args: &Value, f: impl FnOnce(A) -> ToolOutput) -> ToolOutput {
    match parse(name, args) {
        Ok(a) => f(a),
        Err(e) => e,
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n).chain(['…']).collect() } else { s.into() }
}

/// One-line description of a tool call.
pub fn summary(name: &str, args: &Value) -> String {
    let first = |k: &str| args[k].as_str().and_then(|s| s.lines().next()).unwrap_or("");
    let s = match name {
        "read" | "write" => first("path"),
        "glob" | "grep" => first("pattern"),
        "edit" => first("input"),
        "bash" => first("command"),
        "web_search" => first("query"),
        "fetch" => first("url"),
        "wait" => {
            let ids: Vec<&str> = args["ids"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            return if ids.is_empty() { "any".into() } else { ids.join(", ") };
        }
        "task" => {
            let n = args["tasks"].as_array().map_or(0, |t| t.len());
            return format!("{n} subagent{}", if n == 1 { "" } else { "s" });
        }
        _ => "",
    };
    trunc(s, 120)
}
