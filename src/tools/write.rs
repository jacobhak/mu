use super::{ToolOutput, Tools};
use pi_edit::{path_policy::canonical_key, text};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Args {
    path: String,
    content: String,
}

impl Tools {
    pub fn write(&self, a: Args) -> ToolOutput {
        let abs = self.abs(&a.path);
        let disp = self.display(&abs);
        if let Some(parent) = abs.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutput::err(format!("Cannot create {}: {e}", parent.display()));
            }
        }
        if let Err(e) = std::fs::write(&abs, &a.content) {
            return ToolOutput::err(format!("Cannot write {disp}: {e}"));
        }
        let norm = text::normalize_to_lf(text::strip_bom(&a.content).1).into_owned();
        let tag = self.edit_store.record(&canonical_key(&abs), &norm, Some(&[]));
        ToolOutput::ok(format!("[{disp}#{tag}]\nSuccessfully wrote {} bytes to {disp}", a.content.len()))
    }
}
