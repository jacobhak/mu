//! A working-directory switch: the host moves its own copy of the directory and reloads the context
//! (AGENTS.md, rules, skills, commands, agents) for it.

use super::local::{Local, Registry};
use super::{Answer, pending};
use mu_common::paths;
use mu_tools::context::Context;
use mu_tools::mcp::config;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// `cwd`: the new directory. Answers `Switched` once the context is loaded.
pub const SET_CWD: &str = "set_cwd";

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Switched {
    /// False when another switch came while this one's context loaded: the newer one wins.
    pub applied: bool,
    /// Project MCP files in the new directory that stay off until trusted.
    pub untrusted_mcp: usize,
}

/// `partial`: the directory argument of `/cd` typed so far. Answers the directories it can continue to,
/// each as the argument that names it (typed head kept, trailing `/`).
pub const COMPLETE_DIRS: &str = "complete_dirs";

/// The most directories one answer lists.
const MAX_DIRS: usize = 200;

pub(super) fn register(r: &mut Registry) {
    r.add(SET_CWD, set_cwd);
    r.add(COMPLETE_DIRS, complete_dirs);
}

fn complete_dirs(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |partial: String| {
        let cwd = l.cwd();
        async move {
            tokio::task::spawn_blocking(move || dir_candidates(&partial, &cwd, &paths::home()))
                .await
                .map_err(|e| format!("cannot list directories: {e}"))
        }
    })
}

/// The directories `partial` can continue to, relative to `cwd`: those of the directory its last `/`
/// names whose names start with what follows it (case-insensitively). Hidden ones only for a `.` prefix.
fn dir_candidates(partial: &str, cwd: &Path, home: &Path) -> Vec<String> {
    if partial == "~" {
        return vec!["~/".into()];
    }
    let (head, prefix) = partial.split_at(partial.rfind('/').map_or(0, |i| i + 1));
    let dir = match head {
        "" => cwd.to_path_buf(),
        h => paths::expand_tilde(h, home).unwrap_or_else(|| cwd.join(h)),
    };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let lower = prefix.to_lowercase();
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| (!n.starts_with('.') || prefix.starts_with('.')) && n.to_lowercase().starts_with(&lower))
        .collect();
    names.sort();
    names.truncate(MAX_DIRS);
    names.into_iter().map(|n| format!("{head}{n}/")).collect()
}

fn set_cwd(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |cwd: PathBuf| {
        let seq = l.set_cwd(cwd.clone());
        let l = l.clone();
        let skills_user = l.providers.cfg.omp.skills.enable_claude_user;
        async move {
            let loaded = tokio::task::spawn_blocking(move || {
                // `Configs` is not `Sync`; the loader reads only this flag from it.
                let mut cfg = mu_base::config::Configs::default();
                cfg.omp.skills.enable_claude_user = skills_user;
                let untrusted = config::untrusted_files(&cwd, &paths::omp_agent_dir()).len();
                (Context::load_for(&cwd, &cfg), untrusted)
            })
            .await
            .map_err(|e| format!("cannot reload the context: {e}"))?;
            Ok(Switched { applied: l.set_ctx(seq, loaded.0), untrusted_mcp: loaded.1 })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("mu-cd-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["alpha/inner", "Apple", "beta", ".hidden"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("afile"), "").unwrap();
        root
    }

    #[test]
    fn lists_directories_under_the_prefix_and_keeps_what_was_typed() {
        let root = tree();
        let home = root.join("beta");
        let got = |p: &str| dir_candidates(p, &root, &home);
        assert_eq!(got(""), ["Apple/", "alpha/", "beta/"], "files and hidden directories stay out");
        assert_eq!(got("a"), ["Apple/", "alpha/"]);
        assert_eq!(got("alpha/"), ["alpha/inner/"]);
        assert_eq!(got("alpha/in"), ["alpha/inner/"]);
        assert_eq!(got("."), [".hidden/"]);
        assert_eq!(got("nope/"), Vec::<String>::new());
        assert_eq!(got("~"), ["~/"]);
        let abs = root.display();
        assert_eq!(got(&format!("{abs}/a")), [format!("{abs}/Apple/"), format!("{abs}/alpha/")]);
        std::fs::create_dir_all(home.join("under")).unwrap();
        assert_eq!(got("~/"), ["~/under/"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
