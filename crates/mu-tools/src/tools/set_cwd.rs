use super::Tools;
use super::uri::physical;
use mu_base::tool::{ToolDef, ToolOutput};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

#[derive(Deserialize)]
pub struct Args {
    pub(crate) path: String,
}

pub fn def() -> ToolDef {
    ToolDef {
        name: "set_cwd".into(),
        description: "Move the whole session (file tools, bash, status line, project context) to another directory, such as a git worktree of this repository. The user may have to approve it. Relative paths in earlier tool results keep referring to the old directory.".into(),
        schema: json!({"type":"object","properties":{
            "path":{"type":"string","description":"Directory to work in; relative paths resolve against the current one, `~` is home"}},
            "required":["path"]}),
    }
}

impl Tools {
    /// The physical directory `raw` names, whether or not it exists; internal URLs are refused.
    pub(crate) fn set_cwd_target(&self, raw: &str) -> Result<PathBuf, String> {
        let r = self.resolve(raw)?;
        if r.scheme.is_some() {
            return Err(format!("{raw} is not a directory on disk"));
        }
        Ok(physical(&r.path))
    }

    /// Changes nothing itself: the agent switches when it sees `setCwd` in the details, the way it
    /// acts on `enterMode`.
    pub fn set_cwd_tool(&self, a: Args) -> ToolOutput {
        if self.mirror {
            return ToolOutput::err("set_cwd is not available here: this session is mirrored by its parent");
        }
        let target = match self.set_cwd_target(&a.path) {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(e),
        };
        if !target.exists() {
            return ToolOutput::err(format!("{} does not exist", target.display()));
        }
        if !target.is_dir() {
            return ToolOutput::err(format!("{} is not a directory", target.display()));
        }
        let mut o = ToolOutput::ok(format!(
            "Working directory is now {} (was {}). Relative paths in earlier tool results refer to the old directory.",
            target.display(),
            self.cwd.display()
        ));
        o.details = Some(json!({"setCwd": target.to_string_lossy()}));
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(path: &str) -> Args {
        serde_json::from_value(json!({"path": path})).unwrap()
    }

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-set-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("file"), "x").unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn hands_the_physical_directory_to_the_agent() {
        let d = temp();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let out = t.set_cwd_tool(args("sub"));
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(out.details, Some(json!({"setCwd": d.join("sub").to_string_lossy()})));
        assert!(out.text.starts_with(&format!("Working directory is now {}", d.join("sub").display())));
        assert!(out.text.contains(&format!("(was {})", d.display())));
        let up = t.set_cwd_tool(args("sub/.."));
        assert_eq!(up.details, Some(json!({"setCwd": d.to_string_lossy()})));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_missing_path_a_file_and_an_internal_url_are_errors() {
        let d = temp();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        assert!(t.set_cwd_tool(args("nope")).is_error);
        let out = t.set_cwd_tool(args("file"));
        assert!(out.is_error && out.text.contains("not a directory"), "{}", out.text);
        assert!(t.set_cwd_tool(args("local://x")).is_error);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_mirrored_session_refuses() {
        let d = temp();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        t.mirror = true;
        let out = t.set_cwd_tool(args("sub"));
        assert!(out.is_error && out.details.is_none() && out.text.contains("mirrored"), "{}", out.text);
        std::fs::remove_dir_all(&d).ok();
    }
}
