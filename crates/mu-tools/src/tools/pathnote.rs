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

/// The paths an `edit` payload names: `*** Edit File: <path> [all]` (sloppy) and `[<path>#<tag>]` (hashline).
pub(super) fn edit_paths(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in input.lines() {
        let t = line.trim();
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("*** edit file") {
            let rest = t["*** edit file".len()..].trim_start_matches(':').trim();
            let rest = match rest.rsplit_once(char::is_whitespace) {
                Some((p, flag)) if flag.eq_ignore_ascii_case("all") => p.trim_end(),
                _ => rest,
            };
            out.push(rest.trim_matches('"').to_string());
        } else if let Some(inner) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']'))
            && let Some((p, tag)) = inner.rsplit_once('#')
            && !tag.is_empty()
            && tag.chars().all(|c| c.is_ascii_alphanumeric())
        {
            out.push(p.to_string());
        }
    }
    out
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
    fn finds_paths_in_both_edit_formats() {
        let sloppy =
            "*** Edit File: src/a.rs\n*** Find\nx\n*** Replace\ny\n*** Edit File: \"b c.rs\" all\n*** Find\nz\n";
        assert_eq!(edit_paths(sloppy), ["src/a.rs", "b c.rs"]);
        assert_eq!(edit_paths("[crates/x/y.rs#AB12]\n1ab|foo\n"), ["crates/x/y.rs"]);
        assert!(edit_paths("[just a list]\n").is_empty());
    }

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
