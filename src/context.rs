use std::path::{Path, PathBuf};

pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

fn non_empty(p: &Path) -> Option<ContextFile> {
    let content = std::fs::read_to_string(p).ok()?;
    (!content.trim().is_empty()).then(|| ContextFile { path: p.into(), content })
}

/// Project files farthest-first (cwd last), then the user file.
pub fn load(cwd: &Path, home: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut near_first = Vec::new();
    let mut took_omp = false;
    for dir in cwd.ancestors() {
        if cwd.starts_with(home) && dir == home {
            break;
        }
        if dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            continue;
        }
        let omp = (!took_omp).then(|| non_empty(&dir.join(".omp/AGENTS.md"))).flatten();
        if let Some(f) = omp {
            took_omp = true;
            near_first.push(f);
            continue;
        }
        if let Some(f) = non_empty(&dir.join("AGENTS.md")).or_else(|| non_empty(&dir.join("CLAUDE.md"))) {
            near_first.push(f);
        }
    }
    near_first.reverse();
    near_first.extend(non_empty(&agent_dir.join("AGENTS.md")));
    near_first
}

pub fn system_prompt(model: &str, cwd: &Path, files: &[ContextFile]) -> String {
    let os = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let date = jiff::Zoned::now().strftime("%Y-%m-%d");
    let mut s = format!(
        "You are mu, a coding agent running in the user's terminal. Work in the current directory with the provided tools.\n\
- Read a file before editing it. Edit with `edit`, using the `[path#TAG]` header from your latest read, write or edit of that file.\n\
- Use `write` only to create a file or replace it entirely.\n\
- Search with `grep` and `glob` rather than `bash`.\n\
- Be concise; the user sees each tool call.\n\n\
<workstation>\n- OS: linux {}\n- Arch: {}\n- Model: {model}\n- Date: {date}\n- Working directory: {}\n</workstation>\n",
        os.trim(),
        std::env::consts::ARCH,
        cwd.display()
    );
    if !files.is_empty() {
        s.push_str("\n<repo-rules>\nMUST follow these context files for all tasks:\n");
        for f in files {
            s.push_str(&format!("<file path=\"{}\">\n{}\n</file>\n", f.path.display(), f.content.trim_end()));
        }
        s.push_str("</repo-rules>\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn precedence_and_order() {
        let root = std::env::temp_dir().join(format!("mu-ctx-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("a/b");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(root.join("agent")).unwrap();
        fs::write(home.join("AGENTS.md"), "HOME").unwrap();
        fs::write(home.join("a/CLAUDE.md"), "A").unwrap();
        fs::write(proj.join("AGENTS.md"), "B-agents").unwrap();
        fs::write(proj.join("CLAUDE.md"), "B-claude").unwrap();
        fs::write(root.join("agent/AGENTS.md"), "USER").unwrap();
        let got: Vec<_> = load(&proj, &home, &root.join("agent")).into_iter().map(|f| f.content).collect();
        assert_eq!(got, ["A", "B-agents", "USER"]);
    }
}
