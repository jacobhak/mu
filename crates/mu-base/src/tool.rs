//! The tool contract: what a tool is offered as (`ToolDef`) and what it returns (`ToolOutput`).
//! Shared by the tools, providers, plugins and the event protocol; it depends on std, serde and `message` only.
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl ToolDef {
    /// Bytes this definition takes on the wire: its name, description and schema.
    pub fn est_bytes(&self) -> usize {
        self.name.len() + self.description.len() + crate::message::json_len(&self.schema)
    }
}

/// Tokens the tool definitions cost in every request: about 4 bytes per token.
pub fn tool_tokens(defs: &[ToolDef]) -> u64 {
    defs.iter().map(ToolDef::est_bytes).sum::<usize>() as u64 / 4
}

/// Facts about a tool result that the UI needs, recorded by the tool itself so the UI never has
/// to recover them by parsing the text the model sees. The model gets the same text as before.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolMeta {
    Bash(BashMeta),
    /// `read`, `write` and `edit`: the file the call touched.
    File(FileMeta),
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashMeta {
    /// Byte length of the command's own output at the start of the result text; what follows is
    /// notes added by mu (wall time, exit code, spill notice).
    pub output_end: usize,
    pub exit_code: Option<i32>,
    pub wall_secs: Option<f64>,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Number of the artifact holding the full output, when it was cut.
    pub artifact: Option<u32>,
    /// The full-output spill was cut short or could not be written.
    pub spill_note: Option<String>,
    /// Set when the call only started a background job of that id. The command runs on, so the
    /// result carries no output of its own; the job's is shown when it finishes.
    pub background: Option<String>,
}

impl BashMeta {
    /// The command ended abnormally: it exited non-zero, timed out or was cancelled.
    pub fn failed(&self) -> bool {
        self.timed_out || self.cancelled || self.exit_code.is_some_and(|c| c != 0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMeta {
    /// The path as the model wrote it (or `local://…`), shown in the header.
    pub path: String,
    /// The `edit` hash tag of the file after the call.
    pub tag: Option<String>,
    /// A plan file (`local://<slug>-plan.md`): shown as a line count, not a diff.
    pub plan: bool,
}

/// One file a tool wrote, with its text before and after, for front ends that show a real diff.
/// Not stored in the session.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileChange {
    /// Absolute.
    pub path: std::path::PathBuf,
    /// `None` for a file that did not exist.
    pub old: Option<String>,
    /// `None` for a deleted file.
    pub new: Option<String>,
}

/// Texts over this size are left out of a `FileChange`; the change is then not reported.
pub const CHANGE_TEXT_MAX: usize = 512 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    /// UI-only rendering (e.g. a diff); never sent to the model.
    pub display: Option<String>,
    pub details: Option<Value>,
    pub meta: Option<ToolMeta>,
    /// Images the model sees after the text (e.g. `read` of a png). Not shown in the transcript.
    #[serde(skip)]
    pub images: Vec<crate::message::Image>,
    /// Files the call wrote (`write`, `edit`).
    pub changes: Vec<FileChange>,
}

/// Key under which `meta` is stored in a persisted tool result's `details`.
pub(crate) const META_KEY: &str = "muMeta";
/// Larger display diffs are not persisted; a resumed session shows the result text instead.
const STORED_DIFF_MAX: usize = 64 * 1024;

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            display: None,
            details: None,
            meta: None,
            images: Vec::new(),
            changes: Vec::new(),
        }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            display: None,
            details: None,
            meta: None,
            images: Vec::new(),
            changes: Vec::new(),
        }
    }

    /// `details` as stored in the session: the tool's own details plus `meta`, and the display diff
    /// under `diff` (where omp keeps it), so a resumed session still shows it.
    pub fn stored_details(&self) -> Option<Value> {
        if self.meta.is_none() && self.display.is_none() {
            return self.details.clone();
        }
        let mut d = match &self.details {
            Some(Value::Object(m)) => Value::Object(m.clone()),
            None | Some(Value::Null) => Value::Object(Default::default()),
            Some(other) => return Some(other.clone()), // not an object: leave it alone
        };
        if let Some(meta) = &self.meta {
            d[META_KEY] = serde_json::to_value(meta).unwrap_or(Value::Null);
        }
        if let Some(diff) = &self.display
            && diff.len() <= STORED_DIFF_MAX
            && d.get("diff").is_none()
        {
            d["diff"] = Value::String(diff.clone());
        }
        Some(d)
    }

    /// The reverse of `stored_details`: a result rebuilt from a stored message.
    pub fn from_stored(text: String, is_error: bool, details: Option<Value>) -> Self {
        let meta = details.as_ref().and_then(|d| d.get(META_KEY)).and_then(|m| serde_json::from_value(m.clone()).ok());
        let display = details.as_ref().and_then(|d| d.get("diff")).and_then(Value::as_str).map(str::to_string);
        Self { text, is_error, display, details, meta, images: Vec::new(), changes: Vec::new() }
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn tool_definitions_cost_tokens_in_every_request() {
        let def = ToolDef { name: "x".into(), description: "d".repeat(400), schema: serde_json::json!({}) };
        assert!(tool_tokens(&[def]) >= 100);
    }
}
