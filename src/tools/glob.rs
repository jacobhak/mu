use super::grep::walk_options;
use super::{ToolOutput, Tools};
use pi_walker::{CompiledWalkGlob, WalkDetail, WalkFilter, WalkOptions, WalkRequest};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Args {
    pattern: String,
    path: Option<String>,
}

const MAX: usize = 200;

impl Tools {
    pub async fn glob(&self, a: Args) -> ToolOutput {
        let root = self.abs(a.path.as_deref().unwrap_or("."));
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || glob_blocking(&cwd, root, a))
            .await
            .unwrap_or_else(|e| ToolOutput::err(format!("glob failed: {e}")))
    }
}

fn glob_blocking(cwd: &std::path::Path, root: std::path::PathBuf, a: Args) -> ToolOutput {
    let glob = match CompiledWalkGlob::new([a.pattern.as_str()]) {
        Ok(g) => g,
        Err(e) => return ToolOutput::err(format!("Invalid pattern: {e}")),
    };
    let opts = WalkOptions { detail: WalkDetail::Full, ..walk_options() };
    let req = WalkRequest::from_options(&root, opts).filter(WalkFilter::files_only().glob(glob));
    let mut v = match req.collect_files() {
        Ok(v) => v,
        Err(e) => return ToolOutput::err(format!("Walk failed: {e:?}")),
    };
    if v.is_empty() {
        return ToolOutput::ok("No files found matching pattern");
    }
    v.sort_by(|a, b| b.mtime.partial_cmp(&a.mtime).unwrap_or(std::cmp::Ordering::Equal));
    let extra = v.len().saturating_sub(MAX);
    let mut lines: Vec<String> = v.into_iter().take(MAX).map(|e| super::display_in(cwd, &root.join(e.path))).collect();
    if extra > 0 {
        lines.push(format!("… {extra} more"));
    }
    ToolOutput::ok(lines.join("\n"))
}
