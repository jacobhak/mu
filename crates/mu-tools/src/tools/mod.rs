mod ask;
mod diff;
pub mod edit;
mod glob;
pub(crate) mod grep;
mod pathnote;
mod plan;
mod read;
pub(crate) mod shell;
mod spill;
pub mod subagent;
pub mod task;
pub mod todo;
pub mod uri;
pub mod web_gate;
mod write;
mod yield_;

use crate::jobs::JobsHandle;
use crate::plugin::host::PluginHost;
use mu_base::events::EventTx;
use mu_base::text::truncate_text as trunc;
use mu_base::tool::{ToolDef, ToolOutput};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio_util::sync::CancellationToken;

pub struct ToolCtx {
    pub events: EventTx,
    pub cancel: CancellationToken,
    pub call_id: String,
    /// Plan mode is on for this call. The agent owns the plan state and hands it in per call.
    pub plan: bool,
}

/// The per-session state the tools share: the todo list, the session's ids and file, and its skills.
#[derive(Default)]
pub(crate) struct ToolSession {
    todo: std::cell::RefCell<Vec<todo::Phase>>,
    /// Names the session's artifact and local directories, and `PI_SESSION_ID`.
    id: std::cell::RefCell<String>,
    /// `PI_SESSION_FILE`, and the parent of a subagent's session.
    path: std::cell::RefCell<Option<PathBuf>>,
    skills: std::cell::RefCell<Vec<(String, PathBuf)>>,
}

pub struct Tools {
    pub cwd: PathBuf,
    pub plugins: Rc<PluginHost>,
    /// Set in interactive sessions: `task` then runs in the background.
    pub jobs: Option<Rc<JobsHandle>>,
    session: ToolSession,
    /// Set in a subagent started with an `outputSchema`: it must finish through `yield`.
    pub output_schema: std::cell::RefCell<Option<Value>>,
    pub edit_store: pi_edit::EditStore,
    shell: OnceCell<pi_shell::shell::Shell>,
    /// Where the persistent shell was after the last foreground bash call; see `pathnote`.
    shell_cwd: std::cell::RefCell<Option<PathBuf>>,
    /// The persistent shell's exported variables as of the last bash call; `None` before the first.
    shell_env: std::cell::RefCell<Option<std::collections::BTreeMap<String, String>>>,
    /// Test seam: the `__panic` tool panics. Only test builders turn it on.
    #[cfg(any(test, feature = "test-support"))]
    pub panic_tool: std::cell::Cell<bool>,
}

impl Tools {
    #[cfg(test)]
    pub fn for_test() -> Self {
        mu_testutil::home(); // spilled output and image blobs go to a private MU_HOME
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self::new(PathBuf::from("/tmp"), PluginHost::new(vec![], vec![], tx, Default::default()))
    }

    pub fn new(cwd: PathBuf, plugins: Rc<PluginHost>) -> Self {
        Self {
            cwd,
            plugins,
            jobs: None,
            session: Default::default(),
            output_schema: Default::default(),
            edit_store: pi_edit::EditStore::new(),
            shell: OnceCell::new(),
            shell_cwd: Default::default(),
            shell_env: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            panic_tool: Default::default(),
        }
    }

    pub fn shell(&self) -> &pi_shell::shell::Shell {
        self.shell.get_or_init(|| pi_shell::shell::Shell::new(None))
    }

    /// `PI_SESSION_FILE` / `PI_SESSION_ID` for bash commands (omp's names). Always both, empty
    /// without a session, so a value inherited from a parent mu or omp is overwritten.
    pub fn session_env(&self) -> Vec<(&'static str, String)> {
        let file = self.session.path.borrow().as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let id = if file.is_empty() { String::new() } else { self.session.id.borrow().clone() };
        vec![("PI_SESSION_FILE", file), ("PI_SESSION_ID", id)]
    }

    pub fn abs(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { self.cwd.join(p) }
    }

    pub fn display(&self, abs: &Path) -> String {
        display_in(&self.cwd, abs)
    }

    /// Tool definitions that do not depend on the session (the rest are built by `task_def` and
    /// `plan_def`).
    pub fn definitions() -> Vec<ToolDef> {
        BUILTINS.iter().filter_map(|b| b.def.map(|d| d())).collect()
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

    pub fn yield_def(output: &Value) -> ToolDef {
        ToolDef { name: "yield".into(), description: yield_::DESCRIPTION.into(), schema: yield_::schema(output) }
    }

    pub fn plan_def() -> ToolDef {
        ToolDef { name: "propose_plan".into(), description: plan::DESCRIPTION.into(), schema: plan::schema() }
    }

    pub async fn call(&self, name: &str, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        if let Some(raw) = args.get("__invalid_json") {
            return ToolOutput::err(format!("Invalid arguments for {name}: arguments were not valid JSON: {raw}"));
        }
        #[cfg(any(test, feature = "test-support"))]
        if self.panic_tool.get() && name == "__panic" {
            panic!("boom");
        }
        if let Some(b) = BUILTINS.iter().find(|b| b.name == name) {
            return (b.call)(self, args, ctx).await;
        }
        if self.plugins.has_tool(name) {
            if name == "web_search" {
                let store = mu_provider::auth::store::Store::new();
                if let Err(e) = web_gate::gate(&store, self.plugins.is_interactive(), ctx).await {
                    return *e;
                }
            }
            let mut out = self.plugins.call_tool(name, args, &ctx.cancel).await;
            // Only MCP results are capped here: native plugins size their own output (web caps fetches itself).
            if self.plugins.is_mcp_tool(name) {
                out.text = spill::cap_text(std::mem::take(&mut out.text), &self.cwd, &|| self.next_artifact());
            }
            return out;
        }
        ToolOutput::err(format!("Unknown tool: {name}"))
    }
}

/// `abs` as shown to the model: relative to `cwd` when inside it.
pub fn display_in(cwd: &Path, abs: &Path) -> String {
    match abs.strip_prefix(cwd) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        Ok(_) => ".".into(),
        Err(_) => abs.to_string_lossy().into_owned(),
    }
}

type CallFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutput> + 'a>>;

/// One built-in tool: its name, its definition when that does not depend on the session, and its
/// dispatcher. Everything that lists or runs built-ins reads this table.
pub struct Builtin {
    pub name: &'static str,
    pub def: Option<fn() -> ToolDef>,
    call: for<'a> fn(&'a Tools, &'a Value, &'a ToolCtx) -> CallFuture<'a>,
}

fn tool_def(name: &str, description: impl Into<String>, schema: Value) -> ToolDef {
    ToolDef { name: name.into(), description: description.into(), schema }
}

macro_rules! dispatch {
    // Synchronous tool: `|tools, args|`.
    ($name:literal, |$t:ident, $a:ident: $A:ty| $body:expr) => {
        |tools, args, _ctx| {
            Box::pin(async move {
                let $t = tools;
                with_args($name, args, |$a: $A| $body)
            })
        }
    };
    // Async tool with the call context.
    ($name:literal, async |$t:ident, $a:ident: $A:ty, $c:ident| $body:expr) => {
        |tools, args, ctx| {
            Box::pin(async move {
                let ($t, $c) = (tools, ctx);
                match parse::<$A>($name, args) {
                    Ok($a) => $body.await,
                    Err(e) => *e,
                }
            })
        }
    };
}

/// Built-in tools, in the order they are offered to the model.
pub static BUILTINS: &[Builtin] = &[
    Builtin {
        name: "read",
        def: Some(|| {
            tool_def(
                "read",
                read_desc(),
                json!({"type":"object","properties":{
                    "path":{"type":"string","description":"File or directory path, optionally with a selector suffix"}},"required":["path"]}),
            )
        }),
        call: dispatch!("read", async |t, a: read::Args, _c| t.noted(a.paths(), t.read(a))),
    },
    Builtin {
        name: "write",
        def: Some(|| {
            tool_def(
                "write",
                if edit::is_hashline() {
                    "Create a file or replace it entirely. Creates parent directories. Returns a `[path#TAG]` header usable by `edit`."
                } else {
                    "Create a file or replace it entirely. Creates parent directories."
                },
                json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
            )
        }),
        call: dispatch!("write", async |t, a: write::Args, c| t.noted(a.paths(), t.write(a, c.plan))),
    },
    Builtin {
        name: "edit",
        def: Some(|| {
            tool_def(
                "edit",
                pi_edit::description(edit::mode()),
                json!({"type":"object","properties":{
                    "input":{"type":"string","description":"The edit payload, in the format described above"}},"required":["input"]}),
            )
        }),
        call: dispatch!("edit", async |t, a: edit::Args, c| t.noted(a.paths(), t.edit(a, c.plan))),
    },
    Builtin {
        name: "bash",
        def: Some(|| {
            tool_def(
                "bash",
                "Run a shell command: builds, tests, git, real binaries. NEVER use it to read files (use `read`), search (use `grep`/`glob`) or change files (use `edit`/`write`; no `sed`, `perl`, `python` or heredoc edits). The shell persists across calls (cwd and exported variables carry over). Output is truncated in the middle when large. `cmd &` keeps running after the call and shows in `jobs` (stop with `kill %N`); nothing reports its exit. `(cmd &)` ends with the command. For a long command whose result you need later, use `async: true`: it runs as a tracked background job and its result arrives automatically as a system notice. Never poll, sleep or loop for it. An async job starts from the shell's cwd and exported environment as of launch; its own changes do not flow back, and functions and aliases are not carried over. In pty mode, anything left running ends with the command. Stays available in plan mode (as in omp), where you must not change anything with it. NEVER print secret values: check that a variable is set with `[ -n \"$NAME\" ] && echo set` (or `${#NAME}` for its length), and keep tokens out of verbose output (`curl -v`, `set -x`, `git remote -v`). Pass secrets by variable name (`\"$NAME\"`), never by value, and never write them into files, commits or messages.",
                json!({"type":"object","properties":{
                    "command":{"type":"string"},
                    "timeout":{"type":"number","description":"Seconds; default 300, max 3600, 0 disables"},
                    "cwd":{"type":"string","description":"Working directory for this command"},
                    "async":{"type":"boolean","description":"Run as a background job: returns at once and the result arrives automatically. Interactive sessions only (elsewhere it runs in the foreground); not with pty."},
                    "pty":{"type":"boolean","description":"Run in a terminal the user can type into (passwords, sudo, prompts, confirmations). Fresh login shell, not the persistent one. Use only when the command needs user input."}},"required":["command"]}),
            )
        }),
        call: dispatch!("bash", async |t, a: shell::bash::Args, c| t.bash(a, c)),
    },
    Builtin {
        name: "grep",
        def: Some(|| {
            tool_def(
                "grep",
                "Regex search in file contents (respects .gitignore, skips hidden files). Returns `path:line:text` rows; max 20 per file, 200 total.",
                json!({"type":"object","properties":{
                    "pattern":{"type":"string"},"path":{"type":"string","description":"File or directory; default ."},
                    "case":{"type":"boolean","description":"Case-sensitive; default true"}},"required":["pattern"]}),
            )
        }),
        call: dispatch!("grep", async |t, a: grep::Args, _c| t.noted(a.paths(), t.grep(a))),
    },
    Builtin {
        name: "glob",
        def: Some(|| {
            tool_def(
                "glob",
                "Find files by glob pattern (e.g. `**/*.rs`), newest first. Respects .gitignore, skips hidden files.",
                json!({"type":"object","properties":{
                    "pattern":{"type":"string"},"path":{"type":"string","description":"Directory; default ."}},"required":["pattern"]}),
            )
        }),
        call: dispatch!("glob", async |t, a: glob::Args, _c| t.noted(a.paths(), t.glob(a))),
    },
    Builtin {
        name: "todo",
        def: Some(|| tool_def("todo", todo::description(), todo::schema())),
        call: dispatch!("todo", |t, a: todo::Args| t.todo(a)),
    },
    Builtin {
        name: "ask",
        def: Some(|| tool_def("ask", ask::description(), ask::schema())),
        call: dispatch!("ask", async |t, a: ask::Args, c| t.ask(a, c)),
    },
    // Offered per session (plan mode, jobs, custom agents), so no static definition.
    Builtin {
        name: "propose_plan",
        def: None,
        call: dispatch!("propose_plan", async |t, a: plan::Args, c| t.propose_plan(a, c)),
    },
    Builtin { name: "task", def: None, call: dispatch!("task", async |t, a: task::Args, c| t.task(a, c)) },
    Builtin { name: "yield", def: None, call: dispatch!("yield", |t, a: yield_::Args| t.yield_result(a)) },
];

/// Whether `name` is a built-in tool.
pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|b| b.name == name)
}

fn read_desc() -> &'static str {
    if edit::is_hashline() { READ_DESC } else { READ_DESC_PLAIN }
}

const READ_DESC_PLAIN: &str = "Read a file or list a directory. Text files are returned as `N:TEXT` rows, where `N:` is a line number and not part of the file. Images (png, jpeg, gif, webp, up to 5 MB) are returned as images you can see. Default: first 300 lines. Selectors: `path:N` from line N, `path:N-M` range (with 1 line before, 3 after), `path:N+K` K lines from N, `path:-K` last K lines. Long lines are cut at 768 chars; add `:raw` (e.g. `path:N-M:raw`, or a list `path:3,10-12:raw`) to see them in full.";

const READ_DESC: &str = "Read a file or list a directory. Text files are returned as `N:TEXT` rows under a `[path#TAG]` header; the header and line numbers are what `edit` anchors on. Images (png, jpeg, gif, webp, up to 5 MB) are returned as images you can see. Default: first 300 lines. Selectors: `path:N` from line N, `path:N-M` range (with 1 line before, 3 after), `path:N+K` K lines from N, `path:-K` last K lines. Long lines are cut at 768 chars and cannot be edited until seen in full; add `:raw` (e.g. `path:N-M:raw`, or a list `path:3,10-12:raw`) to see them in full.";

fn parse<A: DeserializeOwned>(name: &str, args: &Value) -> Result<A, Box<ToolOutput>> {
    serde_json::from_value(args.clone())
        .map_err(|e| Box::new(ToolOutput::err(format!("Invalid arguments for {name}: {e}"))))
}

fn with_args<A: DeserializeOwned>(name: &str, args: &Value, f: impl FnOnce(A) -> ToolOutput) -> ToolOutput {
    match parse(name, args) {
        Ok(a) => f(a),
        Err(e) => *e,
    }
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
        "review" => first("scope"),
        "task" => {
            let n = args["tasks"].as_array().map_or(0, |t| t.len());
            return format!("{n} subagent{}", if n == 1 { "" } else { "s" });
        }
        _ => "",
    };
    trunc(s, 120)
}

#[cfg(test)]
mod table_tests {
    use super::*;

    #[test]
    fn one_table_drives_names_definitions_and_dispatch() {
        let names: Vec<String> = Tools::definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["read", "write", "edit", "bash", "grep", "glob", "todo", "ask"]);
        assert!(is_builtin("task") && is_builtin("propose_plan") && !is_builtin("wait"));
        assert!(!is_builtin("web_search"));
        // Every built-in dispatches (bad arguments are an argument error, never "Unknown tool").
        let t = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for b in BUILTINS {
            let out = rt.block_on(t.call(b.name, &json!({"__nonsense": true}), &ctx));
            assert!(!out.text.contains("Unknown tool"), "{}: {}", b.name, out.text);
        }
    }
}
