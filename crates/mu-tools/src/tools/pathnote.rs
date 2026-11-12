//! A note for file-tool results when a relative path was resolved against the session directory
//! while the persistent bash shell is somewhere else (after a `cd`).

use super::Tools;
use mu_base::tool::ToolOutput;
use std::future::Future;
use std::path::Path;

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

fn is_relative_file_path(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty() && !p.starts_with('~') && !p.contains("://") && !Path::new(p).is_absolute()
}

impl Tools {
    /// Warns about relative `paths` when the bash shell's directory differs from the session's.
    pub(super) fn cwd_note(&self, paths: &[String]) -> Option<String> {
        let shell = self.shell_cwd.borrow().clone()?;
        if same_dir(&shell, &self.cwd) {
            return None;
        }
        let rel = paths.iter().find(|p| is_relative_file_path(p))?;
        Some(format!(
            "[mu: the relative path `{}` was resolved against the session directory {}, but the bash shell is in {}. \
             If you meant the shell's directory, repeat the call with an absolute path.]",
            rel.trim(),
            self.cwd.display(),
            shell.display()
        ))
    }

    /// Runs a file tool and appends `cwd_note` for `paths` to its result.
    pub(super) async fn noted(&self, paths: Vec<String>, call: impl Future<Output = ToolOutput>) -> ToolOutput {
        let mut out = call.await;
        if let Some(n) = self.cwd_note(&paths) {
            out.text.push_str("\n\n");
            out.text.push_str(&n);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn only_relative_file_paths_count() {
        assert!(is_relative_file_path("src/a.rs"));
        assert!(!is_relative_file_path("/abs/a.rs"));
        assert!(!is_relative_file_path("local://plan.md"));
        assert!(!is_relative_file_path("~/x"));
        assert!(!is_relative_file_path(" "));
    }

    #[test]
    fn the_note_names_both_directories_and_is_silent_when_they_match() {
        let t = Tools::for_test();
        let paths = vec!["a.rs".to_string()];
        assert!(t.cwd_note(&paths).is_none(), "shell location unknown");
        *t.shell_cwd.borrow_mut() = Some(PathBuf::from("/tmp"));
        assert!(t.cwd_note(&paths).is_none(), "same directory");
        *t.shell_cwd.borrow_mut() = Some(PathBuf::from("/usr"));
        let n = t.cwd_note(&paths).unwrap();
        assert!(n.contains("`a.rs`") && n.contains("/tmp") && n.contains("/usr"), "{n}");
        assert!(t.cwd_note(&["/abs".to_string()]).is_none());
    }
}
