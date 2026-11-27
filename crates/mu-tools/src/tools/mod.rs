mod ask;
pub mod changes;
pub mod chart;
mod diff;
pub mod docs;
pub mod edit;
mod enter_mode;
mod glob;
pub(crate) mod grep;
mod job_cancel;
mod job_wait;
mod pathnote;
mod plan;
pub(crate) mod read;
pub mod relay;
mod set_cwd;
pub(crate) mod shell;
mod spill;
pub mod subagent;
pub mod task;
pub mod todo;
pub mod uri;
pub mod web_gate;
pub(crate) mod write;
mod yield_;

use crate::context::{ModeDef, ModelEntry};
use crate::jobs::JobsHandle;
use crate::permissions::Access;
use crate::plugin::host::PluginHost;
use mu_base::events::EventTx;
use mu_base::text::truncate_text as trunc;
use mu_base::tool::{ToolDef, ToolOutput};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio_util::sync::CancellationToken;

pub struct ToolCtx {
    pub events: EventTx,
    pub cancel: CancellationToken,
    pub wait_interrupt: Option<CancellationToken>,
    pub call_id: String,
    /// The active mode, if one is on. The agent owns the mode state and hands it in per call.
    pub mode: Option<ModeDef>,
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
    /// `mu://state/context.md`, built by the agent from what context discovery loaded.
    context_page: std::cell::RefCell<String>,
}

pub struct Tools {
    pub cwd: PathBuf,
    pub plugins: Rc<PluginHost>,
    /// Set in interactive sessions: `task` then runs in the background.
    pub jobs: Option<Rc<JobsHandle>>,
    /// Where `~` points; resolved once from the environment, set directly by tests.
    pub(crate) home: mu_common::paths::Home,
    session: ToolSession,
    /// Set in a subagent started with an `outputSchema`: it must finish through `yield`.
    pub output_schema: std::cell::RefCell<Option<Value>>,
    /// The `@role`s the last advertised `task` def offered, which a task's `model` may name.
    pub delegated: std::cell::RefCell<Vec<String>>,
    /// The model selectors of each custom agent, which a relayed task of that agent may use.
    pub agent_models: std::cell::RefCell<std::collections::HashMap<String, Vec<String>>>,
    /// Each mode's name, how the model may enter it, its description and whether it is read-only;
    /// filled from the session's modes with the tool definitions.
    pub mode_entry: std::cell::RefCell<Vec<(String, ModelEntry, String, bool)>>,
    /// Each mode's exit tool and the mode it belongs to.
    pub mode_tools: std::cell::RefCell<Vec<(String, String)>>,
    pub edit_store: omp_edit::EditStore,
    /// Each read this session (path with its selector) and the fingerprint and count of what it
    /// returned, for the repeated-read hint.
    pub(crate) repeats: std::cell::RefCell<std::collections::HashMap<String, (u64, u32)>>,
    /// The session's tool permissions: the gate reads it, `task` hands it to subagents.
    pub permissions: Rc<crate::permissions::Permissions>,
    /// `[task] launcher` and `[task] launch`: the agent kinds that go through the launcher instead of `mu --headless` here.
    pub launch: Option<subagent::WorkerLaunch>,
    /// This process's session file is mirrored by its parent, so its children's lines go there too.
    pub mirror: bool,
    shell: shell::worker::ShellWorker,
    /// Where the persistent shell was after the last foreground bash call; see `pathnote`.
    shell_cwd: std::cell::RefCell<Option<PathBuf>>,
    /// Set by `set_cwd`: where the persistent shell goes before its next use.
    shell_cd: std::cell::RefCell<Option<PathBuf>>,
    /// The persistent shell's exported variables as of the last bash call; `None` before the first.
    shell_env: std::cell::RefCell<Option<std::collections::BTreeMap<String, String>>>,
    /// Test seam: the `__panic` tool panics. Only test builders turn it on.
    #[cfg(any(test, feature = "test-support"))]
    pub panic_tool: std::cell::Cell<bool>,
    /// Test seam: the executable `task` runs instead of this process. Only test builders set it.
    #[cfg(any(test, feature = "test-support"))]
    pub child_exe: Option<PathBuf>,
}

impl Tools {
    #[cfg(test)]
    pub fn for_test() -> Self {
        mu_testutil::home(); // spilled output and image blobs go to a private MU_HOME
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tools = Self::new(PathBuf::from("/tmp"), PluginHost::new(vec![], vec![], tx, Default::default()));
        tools.shell = shell::worker::ShellWorker::with_grace(std::time::Duration::from_millis(200));
        tools
    }

    pub fn new(cwd: PathBuf, plugins: Rc<PluginHost>) -> Self {
        Self {
            cwd,
            plugins,
            jobs: None,
            home: mu_common::paths::Home::from_env(),
            session: Default::default(),
            output_schema: Default::default(),
            delegated: Default::default(),
            agent_models: Default::default(),
            mode_entry: Default::default(),
            mode_tools: Default::default(),
            edit_store: omp_edit::EditStore::new(),
            repeats: Default::default(),
            permissions: Rc::default(),
            launch: None,
            mirror: false,
            shell: Default::default(),
            shell_cwd: Default::default(),
            shell_cd: Default::default(),
            shell_env: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            panic_tool: Default::default(),
            #[cfg(any(test, feature = "test-support"))]
            child_exe: None,
        }
    }

    pub(crate) fn shell(&self) -> &shell::worker::ShellWorker {
        &self.shell
    }

    /// Moves the session to `dir`: file tools and new commands resolve from it, and the persistent
    /// shell follows before its next use. Running jobs keep the directory they started in.
    pub fn set_cwd(&mut self, dir: PathBuf) {
        *self.shell_cwd.get_mut() = None;
        *self.shell_cd.get_mut() = Some(dir.clone());
        self.cwd = dir;
    }

    /// `PI_SESSION_FILE` / `PI_SESSION_ID` for bash commands (omp's names). Always both, empty
    /// without a session, so a value inherited from a parent mu or omp is overwritten.
    pub fn session_env(&self) -> Vec<(&'static str, String)> {
        let file = self.session.path.borrow().as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let id = if file.is_empty() { String::new() } else { self.session.id.borrow().clone() };
        vec![("PI_SESSION_FILE", file), ("PI_SESSION_ID", id)]
    }

    /// `~` and `~/…` are the home directory; other paths are absolute or relative to `cwd`.
    pub fn abs(&self, p: &str) -> PathBuf {
        if let Some(expanded) = self.home.expand_tilde(p) {
            return expanded;
        }
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

    /// The `bash` tool. `jobs`: background jobs exist (interactive sessions), so every ordinary
    /// command runs in its own shell as a tracked job the model may wait on, and nothing carries over
    /// between calls.
    pub fn bash_def(jobs: bool) -> ToolDef {
        let shell = if jobs {
            " Each call runs in its own shell: pass `cwd` and any variables the command needs, and put setup that must carry over into the same command. `cmd &` and background descendants do not outlive the call. Ordinary commands may wait for their result; use `wait` when a running job's result is needed, and `async: true` for independent concurrent work or long-lived services. A new prompt from the user releases a wait without killing the work."
        } else {
            " The shell persists across calls (cwd and exported variables carry over). Any command that may take longer than ~5 seconds (builds, tests, installs, servers, watches, downloads) MUST use `async: true`, not the foreground: it runs as a tracked background job, and when it finishes mu automatically starts your next turn with its result (no user input needed). Do other work meanwhile; to wait for it, end your turn. Without `async`, `cmd &` keeps running after the call and shows in `jobs` (stop with `kill %N`); nothing reports its exit, so prefer `async`. `(cmd &)` outlives the call too but is not in `jobs`, so nothing can stop it; use `async: true` for anything long-running. An async job starts from the shell's cwd and exported environment as of launch; its own changes do not flow back, and functions and aliases are not carried over."
        };
        let (head, tail) = (spill::HEAD / 1024, spill::TAIL / 1024);
        let cap = head + tail;
        let description = format!(
            "Run a shell command: builds, tests, git, real binaries. NEVER use it to read files (use `read`), search (use `grep`/`glob`) or change files (use `edit`/`write`; no `sed`, `perl`, `python` or heredoc edits).{shell} Output over {cap} KB keeps its first {head} KB and last {tail} KB; the full output is saved to an `artifact://` file you can `read`. NEVER pipe through `tail` or `head` to shorten output: the result then carries their exit code, not the command's. In pty mode, anything left running ends with the command. Stays available in plan mode (as in omp), where you must not change anything with it. NEVER print secret values: check that a variable is set with `[ -n \"$NAME\" ] && echo set` (or `${{#NAME}}` for its length), and keep tokens out of verbose output (`curl -v`, `set -x`, `git remote -v`). Pass secrets by variable name (`\"$NAME\"`), never by value, and never write them into files, commits or messages."
        );
        let async_desc = if jobs {
            "Set for independent concurrent work or a long-lived service: runs as a background job, returns a job id at once, and when it finishes mu starts your next turn with its result; stop it with `job_cancel`. Interactive sessions only (elsewhere it runs in the foreground); not with pty."
        } else {
            "Set for any command that may take more than ~5 seconds: runs as a background job, returns a job id at once, and when it finishes mu starts your next turn with its result; stop it with `job_cancel`. Interactive sessions only (elsewhere it runs in the foreground); not with pty."
        };
        tool_def(
            "bash",
            description,
            json!({"type":"object","properties":{
                "command":{"type":"string"},
                "timeout":{"type":"number","description":"Seconds; default 300, max 3600, 0 disables"},
                "cwd":{"type":"string","description":"Working directory for this command"},
                "async":{"type":"boolean","description":async_desc},
                "notify":{"type":"string","description":"With `async`: a regex (Rust syntax). Each output line that matches reaches you as a system notice while the job runs: it wakes an idle session and ends a blocked `wait`. Lines matching within 200 ms arrive together. Use it to learn that a server is ready or a watcher hit an error without polling; the job's result still arrives when it ends."},
                "pty":{"type":"boolean","description":"Run in a terminal the user can type into (passwords, sudo, prompts, confirmations). Fresh login shell, not the persistent one. Use only when the command needs user input."}},"required":["command"]}),
        )
    }

    /// `async_jobs`: `task` returns job ids immediately instead of blocking, and a job's result may
    /// be awaited with `wait` instead of ending the turn.
    pub fn task_def(async_jobs: bool, agents: &[crate::context::AgentDef], roles: &[&str]) -> ToolDef {
        let mut description = String::from(if async_jobs { task::ASYNC_WAIT_DESCRIPTION } else { task::DESCRIPTION });
        if !agents.is_empty() {
            let list: Vec<String> = agents.iter().map(|a| format!("`{}`: {}", a.name, a.description)).collect();
            description.push_str("\nCustom agents: ");
            description.push_str(&list.join("; "));
        }
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        ToolDef { name: "task".into(), description, schema: task::schema(&names, roles) }
    }

    pub fn yield_def(output: &Value) -> ToolDef {
        ToolDef { name: "yield".into(), description: yield_::DESCRIPTION.into(), schema: yield_::schema(output) }
    }

    /// Offered only where background jobs exist (interactive sessions).
    pub fn job_cancel_def() -> ToolDef {
        ToolDef { name: "job_cancel".into(), description: job_cancel::DESCRIPTION.into(), schema: job_cancel::schema() }
    }

    /// Offered outside a mode, for the modes the model may ask for (`modes`: name and description).
    pub fn enter_mode_def(modes: &[(String, String)]) -> ToolDef {
        enter_mode::def(modes)
    }

    /// Whether `name` is the exit tool of some mode.
    pub fn is_mode_tool(&self, name: &str) -> bool {
        self.mode_tools.borrow().iter().any(|(t, _)| t == name)
    }

    /// How the permission gate judges a tool: a built-in's own declaration, a plugin's manifest, and
    /// anything else (an MCP tool, an unknown name) as one that may change things.
    pub(crate) fn access(&self, name: &str) -> Access {
        if self.is_mode_tool(name) {
            return Access::Read;
        }
        match BUILTINS.iter().find(|b| b.name == name) {
            Some(b) => b.access,
            None => {
                self.plugins.tool_access(name).or_else(|| self.plugins.relay_tool_access(name)).unwrap_or(Access::Exec)
            }
        }
    }

    pub async fn call(&self, name: &str, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        if let Some(raw) = args.get("__invalid_json") {
            return ToolOutput::err(format!("Invalid arguments for {name}: arguments were not valid JSON: {raw}"));
        }
        #[cfg(any(test, feature = "test-support"))]
        if self.panic_tool.get() && name == "__panic" {
            panic!("boom");
        }
        if let Some(m) = ctx.mode.as_ref().filter(|m| m.exit.tool == name) {
            return match parse::<plan::Args>(name, args) {
                Ok(a) => self.propose(a, ctx, m).await,
                Err(e) => *e,
            };
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
        if self.plugins.relay_tool_access(name).is_some() {
            return self.call_relayed(args, ctx).await;
        }
        ToolOutput::err(format!("Unknown tool: {name}"))
    }

    /// A call to a tool whose plugin is on the parent: the parent runs it as a `search`.
    async fn call_relayed(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(relay) = self.plugins.providers().and_then(|p| p.relay()) else {
            return ToolOutput::err("this tool is served by the parent, and there is no relay to it");
        };
        match relay.search(args.clone(), &ctx.cancel).await {
            Ok(r) if r.is_error => ToolOutput::err(r.text),
            Ok(r) => ToolOutput::ok(r.text),
            Err(e) => ToolOutput::err(e.msg),
        }
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
    /// How the permission gate judges it: by its arguments, or as a reader.
    pub access: Access,
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
        access: Access::ByArgs,
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
        access: Access::ByArgs,
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
        call: dispatch!("write", async |t, a: write::Args, c| t.noted(a.paths(), t.write(a, c.mode.as_ref()))),
    },
    Builtin {
        name: "edit",
        access: Access::ByArgs,
        def: Some(|| {
            tool_def(
                "edit",
                omp_edit::description(edit::mode()),
                json!({"type":"object","properties":{
                    "input":{"type":"string","description":"The edit payload, in the format described above"}},"required":["input"]}),
            )
        }),
        call: dispatch!("edit", async |t, a: edit::Args, c| async {
            let paths = a.authored(t);
            t.noted(paths, t.edit(a, c.mode.as_ref().is_some_and(ModeDef::read_only))).await
        }),
    },
    Builtin {
        name: "bash",
        access: Access::ByArgs,
        def: Some(|| Tools::bash_def(false)),
        call: dispatch!("bash", async |t, a: shell::bash::Args, c| t.bash(a, c)),
    },
    Builtin {
        name: "grep",
        access: Access::ByArgs,
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
        access: Access::ByArgs,
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
        name: "set_cwd",
        access: Access::ByArgs,
        def: Some(set_cwd::def),
        call: dispatch!("set_cwd", |t, a: set_cwd::Args| t.set_cwd_tool(a)),
    },
    Builtin {
        name: "todo",
        access: Access::Read,
        def: Some(|| tool_def("todo", todo::description(), todo::schema())),
        call: dispatch!("todo", |t, a: todo::Args| t.todo(a)),
    },
    Builtin {
        name: "ask",
        access: Access::Read,
        def: Some(|| tool_def("ask", ask::description(), ask::schema())),
        call: dispatch!("ask", async |t, a: ask::Args, c| t.ask(a, c)),
    },
    Builtin {
        name: "chart",
        access: Access::Read,
        def: Some(|| tool_def("chart", chart::description(), chart::schema())),
        call: dispatch!("chart", |_t, a: chart::Args| chart::run(a)),
    },
    // Offered per session (modes, jobs, custom agents), so no static definition. A mode's exit tool
    // is not here: `Tools::call` routes it by the active mode.
    Builtin {
        name: "enter_mode",
        access: Access::Read,
        def: None,
        call: dispatch!("enter_mode", |t, a: enter_mode::Args| t.enter_mode(a)),
    },
    Builtin {
        name: "task",
        access: Access::Read,
        def: None,
        call: dispatch!("task", async |t, a: task::Args, c| t.task(a, c)),
    },
    Builtin {
        name: "job_cancel",
        access: Access::Read,
        def: None,
        call: dispatch!("job_cancel", |t, a: job_cancel::Args| t.job_cancel(a)),
    },
    Builtin {
        name: "wait",
        access: Access::Read,
        def: None,
        call: dispatch!("wait", async |t, a: job_wait::Args, c| t.job_wait(a, c)),
    },
    Builtin {
        name: "yield",
        access: Access::Read,
        def: None,
        call: dispatch!("yield", |t, a: yield_::Args| t.yield_result(a)),
    },
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
        "read" | "write" | "set_cwd" => first("path"),
        "glob" | "grep" => first("pattern"),
        "edit" => first("input"),
        "bash" => first("command"),
        "job_cancel" if args["all"].as_bool() == Some(true) => "all",
        "job_cancel" => first("id"),
        "wait" => {
            let ids: Vec<&str> = args["ids"].as_array().into_iter().flatten().filter_map(|i| i.as_str()).collect();
            return if ids.is_empty() { "any".into() } else { trunc(&ids.join(", "), 120) };
        }
        "web_search" => first("query"),
        "chart" => first("title"),
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
mod path_tests {
    use super::*;
    use mu_common::paths::Home;

    #[test]
    fn tilde_paths_are_home_relative() {
        let home = std::env::temp_dir().join(format!("mu-home-{}", uuid::Uuid::new_v4()));
        let mut t = Tools::for_test();
        t.cwd = PathBuf::from("/work");
        t.home = Home::at(&home);
        assert_eq!(t.abs("~/x"), home.join("x"));
        assert_eq!(t.abs("~"), home);
        assert_eq!(t.abs("~user/x"), PathBuf::from("/work/~user/x"));
        assert_eq!(t.abs("a/b"), PathBuf::from("/work/a/b"));
        assert_eq!(t.abs("/etc/x"), PathBuf::from("/etc/x"));
        t.home = Home::at("");
        assert_eq!(t.abs("~/x"), PathBuf::from("/work/~/x"), "an empty HOME leaves the path alone");
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;

    #[test]
    fn one_table_drives_names_definitions_and_dispatch() {
        let names: Vec<String> = Tools::definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["read", "write", "edit", "bash", "grep", "glob", "set_cwd", "todo", "ask", "chart"]);
        assert!(is_builtin("task") && is_builtin("enter_mode") && is_builtin("wait"));
        assert!(!is_builtin("propose_plan"), "an exit tool belongs to a mode, not the table");
        assert!(!names.contains(&"wait".to_string()), "wait is session-only");
        assert!(!is_builtin("web_search"));
        // Every built-in dispatches (bad arguments are an argument error, never "Unknown tool").
        let t = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), wait_interrupt: None, call_id: "c".into(), mode: None };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for b in BUILTINS {
            let out = rt.block_on(t.call(b.name, &json!({"__nonsense": true}), &ctx));
            assert!(!out.text.contains("Unknown tool"), "{}: {}", b.name, out.text);
        }
    }

    /// The gate has an argument reading for exactly these tools; any other built-in declares its tier.
    #[test]
    fn only_the_file_and_shell_tools_are_judged_by_their_arguments() {
        let by_args: Vec<&str> = BUILTINS.iter().filter(|b| b.access == Access::ByArgs).map(|b| b.name).collect();
        assert_eq!(by_args, ["read", "write", "edit", "bash", "grep", "glob", "set_cwd"]);
        assert!(
            BUILTINS.iter().all(|b| b.access != Access::Exec),
            "a built-in that may act is judged by its arguments"
        );
    }
}
