use super::uri::Scheme;
use super::{Tools, diff};
use mu_base::tool::{CHANGE_TEXT_MAX, FileChange, FileMeta, ToolMeta, ToolOutput};
use pi_edit::{path_policy::canonical_key, text};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Args {
    path: String,
    content: String,
}

impl Tools {
    pub async fn write(&self, a: Args, plan: bool) -> ToolOutput {
        let res = match self.resolve(&a.path) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        if let Some(s @ (Scheme::Artifact | Scheme::Skill)) = res.scheme {
            return ToolOutput::err(format!("{}:// is read-only", s.name()));
        }
        if plan && res.scheme != Some(Scheme::Local) {
            return ToolOutput::err(
                "Plan mode: the working tree is read-only. Write your plan to a local://<slug>-plan.md file instead.",
            );
        }
        let abs = res.path;
        let disp = if res.scheme.is_some() { a.path.clone() } else { self.display(&abs) };
        // The filesystem work runs on a blocking thread; `content` moves in and comes back.
        let (path, content) = (abs.clone(), a.content);
        let done = tokio::task::spawn_blocking(move || -> Result<(Option<String>, String), String> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
            }
            let old = std::fs::read_to_string(&path).ok();
            std::fs::write(&path, &content).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
            Ok((old, content))
        })
        .await;
        let (old, content) = match done {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return ToolOutput::err(e.replace(&abs.display().to_string(), &disp)),
            Err(e) => return ToolOutput::err(format!("write failed: {e}")),
        };
        let a = Args { path: a.path, content };
        let norm = text::normalize_to_lf(text::strip_bom(&a.content).1).into_owned();
        let tag = self.edit_store.record(&canonical_key(&abs), &norm, Some(&[]));
        let mut out = ToolOutput::ok(format!("[{disp}#{tag}]\nSuccessfully wrote {} bytes to {disp}", a.content.len()));
        let plan = a.path.starts_with("local://") && a.path.ends_with("-plan.md");
        out.meta = Some(ToolMeta::File(FileMeta { path: disp.clone(), tag: Some(tag), plan }));
        let d = diff::unified(old.as_deref().unwrap_or_default(), &a.content);
        if !d.is_empty() {
            out.display = Some(d);
        }
        if old.as_ref().is_none_or(|o| o.len() <= CHANGE_TEXT_MAX) && a.content.len() <= CHANGE_TEXT_MAX {
            out.changes = vec![FileChange { path: abs, old, new: Some(a.content) }];
        }
        out
    }
}
