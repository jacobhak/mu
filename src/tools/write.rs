use super::uri::Scheme;
use super::{ToolOutput, Tools, diff};
use pi_edit::{path_policy::canonical_key, text};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Args {
    path: String,
    content: String,
}

impl Tools {
    pub fn write(&self, a: Args) -> ToolOutput {
        let res = match self.resolve(&a.path) {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(e),
        };
        if let Some(s @ (Scheme::Artifact | Scheme::Skill)) = res.scheme {
            return ToolOutput::err(format!("{}:// is read-only", s.name()));
        }
        if self.plan.get() && res.scheme != Some(Scheme::Local) {
            return ToolOutput::err(
                "Plan mode: the working tree is read-only. Write your plan to a local://<slug>-plan.md file instead.",
            );
        }
        let abs = res.path;
        let disp = if res.scheme.is_some() { a.path.clone() } else { self.display(&abs) };
        if let Some(parent) = abs.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutput::err(format!("Cannot create {}: {e}", parent.display()));
            }
        }
        let old = std::fs::read_to_string(&abs).unwrap_or_default();
        if let Err(e) = std::fs::write(&abs, &a.content) {
            return ToolOutput::err(format!("Cannot write {disp}: {e}"));
        }
        let norm = text::normalize_to_lf(text::strip_bom(&a.content).1).into_owned();
        let tag = self.edit_store.record(&canonical_key(&abs), &norm, Some(&[]));
        let mut out = ToolOutput::ok(format!("[{disp}#{tag}]\nSuccessfully wrote {} bytes to {disp}", a.content.len()));
        let d = diff::unified(&old, &a.content);
        if !d.is_empty() {
            out.display = Some(d);
        }
        out
    }
}
