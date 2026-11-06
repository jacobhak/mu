use super::Tools;
use crate::walk::{block_on_walk, walk_options};
use mu_base::tool::ToolOutput;
use pi_walker::glob::{GlobError, GlobOptions, glob};
use pi_walker::{FileType, Fs};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Args {
    pattern: String,
    path: Option<String>,
}

impl Args {
    pub(super) fn paths(&self) -> Vec<String> {
        vec![self.path.clone().unwrap_or_else(|| ".".into())]
    }
}

const MAX: usize = 200;

impl Tools {
    pub async fn glob(&self, a: Args) -> ToolOutput {
        let (root, prefix) = match self.search_root(a.path.as_deref()) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || glob_blocking(&cwd, root, prefix, a))
            .await
            .unwrap_or_else(|e| ToolOutput::err(format!("glob failed: {e}")))
    }
}

fn glob_blocking(cwd: &std::path::Path, root: std::path::PathBuf, prefix: Option<String>, a: Args) -> ToolOutput {
    // Non-recursive: the pattern applies to the root-relative path as written (`*.rs` is top level only).
    let opts = GlobOptions {
        pattern: a.pattern,
        file_type: Some(FileType::File),
        sort_by_mtime: true,
        include_node_modules: Some(true),
        walk: walk_options(),
        ..Default::default()
    };
    let v = match block_on_walk(glob(&Fs::native(), &root, &opts)) {
        Ok(v) => v,
        Err(GlobError::Pattern(e)) => return ToolOutput::err(format!("Invalid pattern: {e}")),
        Err(GlobError::Walk(e)) => return ToolOutput::err(format!("Walk failed: {e}")),
    };
    if v.is_empty() {
        return ToolOutput::ok("No files found matching pattern");
    }
    let extra = v.len().saturating_sub(MAX);
    let mut lines: Vec<String> =
        v.into_iter().take(MAX).map(|e| super::uri::shown(cwd, &root, prefix.as_deref(), &root.join(e.path))).collect();
    if extra > 0 {
        lines.push(format!("… {extra} more"));
    }
    ToolOutput::ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn a_root_inside_a_hidden_directory_is_still_globbed() {
        let d = std::env::temp_dir().join(format!("mu-walk-{}", uuid::Uuid::new_v4())).join(".hid/docs");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.md"), "x\n").unwrap();
        let t = Tools::for_test();
        let o = t.glob(Args { pattern: "*.md".into(), path: Some(d.display().to_string()) }).await;
        assert!(o.text.ends_with("a.md"), "{}", o.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pattern_is_root_relative_and_newest_first() {
        let d = super::super::grep::tests::tree();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(d.join("a.rs")).unwrap().set_modified(old).unwrap();
        let mut t = Tools::for_test();
        t.cwd = d.clone();
        let o = t.glob(Args { pattern: "*.rs".into(), path: None }).await;
        assert_eq!(o.text, "big.rs\na.rs");
        let o = t.glob(Args { pattern: "**/b.rs".into(), path: None }).await;
        assert_eq!(o.text, "sub/b.rs");
        let o = t.glob(Args { pattern: "**/*.rs".into(), path: None }).await;
        assert_eq!(o.text.lines().last(), Some("a.rs"), "{}", o.text);
        assert!(!o.text.contains("gen.rs") && !o.text.contains("c.rs"), "{}", o.text);
        let o = t.glob(Args { pattern: "[".into(), path: None }).await;
        assert!(o.is_error && o.text.starts_with("Invalid pattern"), "{}", o.text);
    }
}
