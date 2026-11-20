//! The subjects of one call: what it reads, writes or runs, each with what the mode must grant for it.

use super::bash::{BashAnalysis, analyze};
use super::verdict::{Gate, Subject};
use super::{Access, Permissions};
use crate::tools::Tools;
use crate::tools::uri::physical;
use mu_base::permissions::Capability;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(super) struct Call {
    pub subjects: Vec<Subject>,
    pub bash: Option<BashAnalysis>,
    /// The directory a bash call starts in.
    pub bash_dir: PathBuf,
}

/// The subjects of one call; `Err(())` when the arguments do not fit the tool, which is the tool's
/// own error to report. A tool declares whether it only reads, may change things, or is judged from
/// its arguments.
pub(super) fn classify(tools: &Tools, gate: &Gate, name: &str, args: &Value) -> Result<Call, ()> {
    let access = tools.access(name);
    if access == Access::ByArgs
        && let Some(call) = by_args(tools, gate, name, args)?
    {
        return Ok(call);
    }
    let text = if name == "fetch" { args["url"].as_str().unwrap_or("") } else { "" };
    let needs = match access {
        Access::Read if name == "fetch" && !gate.searched_url(text) => Capability::Network,
        Access::Read => Capability::ReadProject,
        _ => Capability::Exec,
    };
    let subject = Subject::new(name, text, needs);
    Ok(Call { subjects: vec![subject], bash: None, bash_dir: tools.cwd.clone() })
}

impl Permissions {
    /// Whether a call only reads and may run beside other such calls: the file readers, a bash line
    /// of read-only commands (no `cd`, assignment, redirect, `pty` or `async`), and a plugin tool
    /// whose manifest tier is `read`. Everything else (`ask`, `task`, `web_search`, which may prompt,
    /// the other built-ins) runs alone. Whether the call needs approval is `check`'s verdict, not this.
    pub fn runs_concurrently(&self, tools: &Tools, name: &str, args: &Value) -> bool {
        match name {
            "read" | "grep" | "glob" => true,
            "bash" => {
                if args["pty"].as_bool() == Some(true) || args["async"].as_bool() == Some(true) {
                    return false;
                }
                let Some(cmd) = args["command"].as_str() else { return false };
                let a = analyze(cmd);
                a.parsed
                    && !a.cmds.is_empty()
                    && a.unplaced_from.is_none()
                    && a.cmds.iter().all(|c| c.readonly && !c.assigns && c.writes.is_empty())
            }
            "web_search" => false,
            _ => !crate::tools::is_builtin(name) && tools.access(name) == Access::Read,
        }
    }
}

/// The file and shell tools, judged from their arguments; `None` for a name that is not one of them.
fn by_args(tools: &Tools, gate: &Gate, name: &str, args: &Value) -> Result<Option<Call>, ()> {
    let mut call = Call { subjects: Vec::new(), bash: None, bash_dir: tools.cwd.clone() };
    if name == "bash" {
        let Some(cmd) = args["command"].as_str() else { return Err(()) };
        let pty = args["pty"].as_bool() == Some(true);
        let dir = tools.bash_dir(args["cwd"].as_str(), pty, args["async"].as_bool() == Some(true));
        let analysis = analyze(cmd);
        for (i, c) in analysis.cmds.iter().enumerate() {
            let needs = if gate.readonly_bash && analysis.parsed && c.readonly {
                let outside = c.paths.iter().any(|w| word_outside(gate, &dir, &analysis, i, w.prefix.as_deref()));
                if outside { Capability::ReadOutside } else { Capability::ReadProject }
            } else {
                Capability::Exec
            };
            call.subjects.push(Subject {
                no_allow: c.assigns || !analysis.parsed,
                also: c.forms.clone(),
                ..Subject::new(name, c.text.clone(), needs)
            });
        }
        if analysis.cmds.is_empty() {
            // A line that holds no command we can see is one the shell may read differently.
            call.subjects.push(Subject::new(name, cmd, Capability::Exec));
        }
        // An output redirect is a write of its own, whatever command carries it.
        let mut seen: Vec<(&str, Option<PathBuf>)> = Vec::new();
        for (i, c) in analysis.cmds.iter().enumerate() {
            for w in &c.writes {
                let at = w.path.as_deref().filter(|p| placed(&analysis, i, p)).map(|p| place(&dir, p));
                if seen.iter().any(|(t, a)| *t == w.text && *a == at) {
                    continue;
                }
                call.subjects.push(match &at {
                    Some(p) => gate.write_subject("write", p),
                    None => Subject { no_allow: true, ..Subject::new("write", w.text.clone(), Capability::Exec) },
                });
                seen.push((&w.text, at));
            }
        }
        if pty {
            // The login shell's grammar is not the classifier's: nothing it says is trusted.
            for s in &mut call.subjects {
                s.needs = Capability::Exec;
                s.no_allow = true;
            }
        }
        call.bash = Some(analysis);
        call.bash_dir = dir;
        return Ok(Some(call));
    }
    if name == "write" {
        let a: crate::tools::write::Args = serde_json::from_value(args.clone()).map_err(|_| ())?;
        for raw in a.paths() {
            call.subjects.push(match tools.resolve(&raw) {
                Ok(r) if r.scheme.is_some() => Subject::new(name, raw, Capability::ReadProject),
                Ok(r) => gate.write_subject(name, &physical(&r.path)),
                Err(_) => Subject::new(name, raw, Capability::Exec),
            });
        }
        return Ok(Some(call));
    }
    if name == "edit" {
        let a: crate::tools::edit::Args = serde_json::from_value(args.clone()).map_err(|_| ())?;
        let targets = a.targets(tools).unwrap_or_default();
        for t in targets {
            call.subjects.push(match t.abs {
                Some(abs) if !t.internal => gate.write_subject(name, &physical(&abs)),
                // Internal pages stay inside mu's own; omp-edit refuses a target it cannot resolve.
                _ => Subject::new(name, t.authored, Capability::ReadProject),
            });
        }
        if call.subjects.is_empty() {
            // A payload that names no target we can read is one the editor may read differently.
            call.subjects.push(Subject { no_allow: true, ..Subject::new(name, "", Capability::Exec) });
        }
        return Ok(Some(call));
    }
    if matches!(name, "read" | "grep" | "glob") {
        let raw = match name {
            "read" => {
                let a: crate::tools::read::Args = serde_json::from_value(args.clone()).map_err(|_| ())?;
                tools.read_file(a.path()).path
            }
            _ => args["path"].as_str().unwrap_or(".").to_string(),
        };
        call.subjects.push(match tools.resolve(&raw) {
            Ok(r) if r.scheme.is_some() => Subject::new(name, raw, Capability::ReadProject),
            Ok(r) => gate.read_subject(name, &physical(&r.path)),
            Err(_) => Subject::new(name, raw, Capability::ReadOutside),
        });
        return Ok(Some(call));
    }
    Ok(None)
}

/// The file a word of a bash command names, from the directory the command runs in.
pub(super) fn place(dir: &Path, word: &str) -> PathBuf {
    physical(&if Path::new(word).is_absolute() { PathBuf::from(word) } else { dir.join(word) })
}

/// Whether command `i` runs where `place` expects it: an absolute word does not care.
fn placed(a: &BashAnalysis, i: usize, word: &str) -> bool {
    Path::new(word).is_absolute() || a.unplaced_from.is_none_or(|n| i < n)
}

/// Whether a path word of command `i` may point outside the project: an expansion decides it, an
/// earlier `cd` moved the command, or it lies there.
pub(super) fn word_outside(gate: &Gate, dir: &Path, a: &BashAnalysis, i: usize, prefix: Option<&str>) -> bool {
    match prefix {
        Some(p) => !placed(a, i, p) || !gate.inside(&place(dir, p)),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_readers_run_concurrently() {
        let (t, p) = (Tools::for_test(), Permissions::default());
        let runs = |name: &str, args: Value| p.runs_concurrently(&t, name, &args);
        assert!(runs("read", json!({"path": "a"})) && runs("grep", json!({"pattern": "a"})));
        assert!(runs("bash", json!({"command": "ls | wc -l"})));
        for command in ["touch a", "ls > out", "cd src && ls", "X=1 ls", "ls $(touch a)"] {
            assert!(!runs("bash", json!({ "command": command })), "{command}");
        }
        assert!(!runs("bash", json!({"command": "ls", "async": true})));
        assert!(!runs("bash", json!({"command": "ls", "pty": true})));
        for name in ["write", "edit", "todo", "ask", "task", "wait", "yield", "web_search", "mcp__x__y"] {
            assert!(!runs(name, json!({})), "{name}");
        }
    }
}
