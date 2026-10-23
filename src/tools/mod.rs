mod bash;
mod edit;
mod glob;
pub mod grep;
mod read;
mod write;

use pi_shell::cancel::{AbortReason, AbortToken};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::cell::{OnceCell, RefCell};
use std::path::{Path, PathBuf};

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true }
    }
}

/// Lets Ctrl-C reach an in-flight bash run.
#[derive(Default)]
pub struct AbortHandle(RefCell<Option<AbortToken>>, std::cell::Cell<bool>);

impl AbortHandle {
    pub fn set(&self, t: AbortToken) {
        self.1.set(false);
        *self.0.borrow_mut() = Some(t);
    }
    pub fn fired(&self) -> bool {
        self.1.get()
    }
    pub fn clear(&self) {
        *self.0.borrow_mut() = None;
    }
    pub fn fire(&self) {
        self.1.set(true);
        if let Some(t) = self.0.borrow().as_ref() {
            t.abort(AbortReason::Signal);
        }
    }
}

pub struct Tools {
    pub cwd: PathBuf,
    pub edit_store: pi_edit::EditStore,
    shell: OnceCell<pi_shell::shell::Shell>,
}

impl Tools {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd, edit_store: pi_edit::EditStore::new(), shell: OnceCell::new() }
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

    pub fn definitions() -> Vec<Value> {
        vec![
            json!({"name":"read","description":READ_DESC,"input_schema":{"type":"object","properties":{
                "path":{"type":"string","description":"File or directory path, optionally with a selector suffix"}},"required":["path"]}}),
            json!({"name":"write","description":"Create a file or replace it entirely. Creates parent directories. Returns a `[path#TAG]` header usable by `edit`.","input_schema":{"type":"object","properties":{
                "path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}),
            json!({"name":"edit","description":pi_edit::description(pi_edit::EditMode::Hashline),"input_schema":{"type":"object","properties":{
                "input":{"type":"string","description":"The edit payload, in the format described above"}},"required":["input"]}}),
            json!({"name":"bash","description":"Run a shell command. The shell persists across calls (cwd and exported variables carry over). Output is truncated in the middle when large.","input_schema":{"type":"object","properties":{
                "command":{"type":"string"},
                "timeout":{"type":"number","description":"Seconds; default 300, max 3600, 0 disables"},
                "cwd":{"type":"string","description":"Working directory for this command"}},"required":["command"]}}),
            json!({"name":"grep","description":"Regex search in file contents (respects .gitignore, skips hidden files). Returns `path:line:text` rows; max 20 per file, 200 total.","input_schema":{"type":"object","properties":{
                "pattern":{"type":"string"},"path":{"type":"string","description":"File or directory; default ."},
                "case":{"type":"boolean","description":"Case-sensitive; default true"}},"required":["pattern"]}}),
            json!({"name":"glob","description":"Find files by glob pattern (e.g. `**/*.rs`), newest first. Respects .gitignore, skips hidden files.","input_schema":{"type":"object","properties":{
                "pattern":{"type":"string"},"path":{"type":"string","description":"Directory; default ."}},"required":["pattern"]}}),
        ]
    }

    pub async fn call(&self, name: &str, args: &Value, abort: &AbortHandle) -> ToolOutput {
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
            "bash" => match parse::<bash::Args>(name, args) {
                Ok(a) => self.bash(a, abort).await,
                Err(e) => e,
            },
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
