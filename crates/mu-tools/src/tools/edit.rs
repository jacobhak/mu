use super::Tools;
use super::uri::Scheme;
use async_trait::async_trait;
use mu_base::tool::{CHANGE_TEXT_MAX, FileChange, FileMeta, ToolMeta, ToolOutput};
use pi_edit::{
    ApplyRequest, EditError, EditMode, EditResult, EditWriter, PathPolicy, Session, UrlResolution, WriteRequest,
    WriteResponse, engine::FileOp, session::SessionConfig,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct Args {
    input: String,
}

impl Args {
    pub(super) fn paths(&self) -> Vec<String> {
        super::pathnote::edit_paths(&self.input)
    }
}

/// The first line that looks like a sloppy-format header but is not one. The parser reads such a line as
/// body text, so `*** Find all` would be written into the file. Only unindented lines count: an indented
/// one is how a literal `*** …` line is written.
fn near_miss_header(input: &str) -> Option<(usize, &str)> {
    input.lines().enumerate().find_map(|(i, line)| {
        let rest = line.strip_prefix("***")?;
        let spaced = rest.starts_with([' ', '\t']);
        let body = rest.trim();
        let lower = body.to_ascii_lowercase();
        let word = |w: &str| lower.strip_prefix(w).filter(|t| t.is_empty() || t.starts_with(char::is_whitespace));
        let op = ["find", "replace", "insert before", "insert after"].iter().find_map(|w| word(w));
        let bad = match op {
            // An operator header takes no argument.
            Some(tail) => !spaced || !tail.trim().is_empty(),
            None => {
                !spaced && ["edit file", "find", "replace", "insert"].iter().any(|w| lower.starts_with(w))
                    || spaced && word("all").is_some()
                    || spaced && word("insert").is_some()
            }
        };
        bad.then_some((i + 1, line.trim_end()))
    })
}

fn refuse_near_miss(mode: EditMode, input: &str) -> Option<ToolOutput> {
    if mode != EditMode::Sloppy {
        return None;
    }
    let (n, line) = near_miss_header(input)?;
    Some(ToolOutput::err(format!(
        "Line {n}, `{line}`, looks like an edit header but is not one, so it would be written into the file as \
         text. Headers are `*** Edit File: <path>` (with ` all` after the path to change every match), \
         `*** Find`, `*** Replace`, `*** Insert Before` and `*** Insert After`, each alone on its line. If the \
         line is meant as text, indent it. Nothing was changed."
    )))
}

#[cfg(test)]
impl Args {
    pub fn for_test(input: String) -> Self {
        Self { input }
    }
}

static MODE: std::sync::OnceLock<EditMode> = std::sync::OnceLock::new();

/// Set the edit format once at startup from `[edit] mode` (`MU_EDIT_MODE` overrides). Only the
/// formats that take a single `input` string are offered: `hashline` (default) and `sloppy`.
pub fn set_mode(configured: &str) -> Result<(), String> {
    let name = std::env::var("MU_EDIT_MODE").unwrap_or_else(|_| configured.to_string());
    let m = match name.trim() {
        "" | "hashline" => EditMode::Hashline,
        "sloppy" => EditMode::Sloppy,
        other => return Err(format!("unknown edit mode {other:?}; use \"hashline\" or \"sloppy\"")),
    };
    let _ = MODE.set(m);
    Ok(())
}

pub fn mode() -> EditMode {
    *MODE.get().unwrap_or(&EditMode::Hashline)
}

pub fn is_hashline() -> bool {
    mode() == EditMode::Hashline
}

/// Writes the files an edit changes and keeps each file's text before and after.
#[derive(Default)]
struct FsWriter {
    changes: std::sync::Mutex<Vec<FileChange>>,
}

fn werr(e: impl std::fmt::Display) -> EditError {
    EditError::Writer(e.to_string())
}

/// A file's text: `Some(None)` when it does not exist, `None` when it cannot be kept (binary or large).
async fn snapshot(p: &std::path::Path) -> Option<Option<String>> {
    match tokio::fs::metadata(p).await {
        Err(_) => Some(None),
        Ok(m) if m.len() as usize > CHANGE_TEXT_MAX => None,
        Ok(_) => tokio::fs::read_to_string(p).await.ok().map(Some),
    }
}

impl FsWriter {
    fn record(&self, path: &std::path::Path, old: Option<Option<String>>, new: Option<&str>) {
        let Some(old) = old else { return };
        if new.is_some_and(|n| n.len() > CHANGE_TEXT_MAX) {
            return;
        }
        if let Ok(mut c) = self.changes.lock() {
            c.push(FileChange { path: path.to_path_buf(), old, new: new.map(String::from) });
        }
    }
}

#[async_trait]
impl EditWriter for FsWriter {
    async fn write(&self, r: WriteRequest) -> EditResult<WriteResponse> {
        match r.op {
            FileOp::Delete => {
                let old = snapshot(&r.absolute).await;
                tokio::fs::remove_file(&r.absolute).await.map_err(werr)?;
                self.record(&r.absolute, old, None);
                Ok(WriteResponse::default())
            }
            FileOp::Noop => Ok(WriteResponse::default()),
            FileOp::Create | FileOp::Update => {
                let content = r.content.ok_or_else(|| werr("edit request omitted content"))?;
                let dest = r.move_to.as_deref().unwrap_or(&r.absolute);
                let old = snapshot(&r.absolute).await;
                if let Some(p) = dest.parent() {
                    tokio::fs::create_dir_all(p).await.map_err(werr)?;
                }
                tokio::fs::write(dest, &content).await.map_err(werr)?;
                if r.move_to.is_some() {
                    if let Ok(m) = tokio::fs::metadata(&r.absolute).await {
                        let _ = tokio::fs::set_permissions(dest, m.permissions()).await;
                    }
                    tokio::fs::remove_file(&r.absolute).await.map_err(werr)?;
                }
                self.record(dest, old, Some(&content));
                Ok(WriteResponse { written: content, ..Default::default() })
            }
        }
    }
}

impl Tools {
    pub async fn edit(&self, a: Args, plan: bool) -> ToolOutput {
        if let Some(refusal) = refuse_near_miss(mode(), &a.input) {
            return refusal;
        }
        let policy = PathPolicy {
            cwd: self.cwd.clone(),
            home_dir: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
            url_schemes: vec!["local".into(), "artifact".into(), "skill".into()],
            url_alias_schemes: vec![],
            plan_writable_roots: vec![self.local_root()],
            plan_active: plan,
            block_auto_generated: true,
        };
        let cfg = SessionConfig {
            mode: mode(),
            policy,
            allow_fuzzy: true,
            fuzzy_threshold: 0.95,
            enforce_seen_lines: true,
            max_columns: 768,
            raw_input: false,
        };
        let mut s = Session::new(cfg, self.edit_store.clone());
        s.set_args_json(&json!({"input": a.input}).to_string());
        s.finish();
        for url in s.begin_apply_url_targets() {
            let r = match self.resolve(&url) {
                Ok(r) => match r.scheme {
                    Some(Scheme::Local) => UrlResolution { absolute: Some(r.path), error: None, plan_writable: true },
                    Some(sc) => UrlResolution {
                        absolute: None,
                        error: Some(format!("{}:// is read-only", sc.name())),
                        plan_writable: false,
                    },
                    None => UrlResolution { absolute: Some(r.path), error: None, plan_writable: false },
                },
                Err(e) => UrlResolution { absolute: None, error: Some(e), plan_writable: false },
            };
            s.provide(url, r);
        }
        let writer = FsWriter::default();
        match s.apply(ApplyRequest::default(), &writer).await {
            Ok(o) => {
                let mut out = ToolOutput::ok(o.text);
                out.meta = o
                    .files
                    .first()
                    .map(|f| ToolMeta::File(FileMeta { path: f.display.clone(), tag: None, plan: false }));
                let diffs: Vec<&str> = o.files.iter().map(|f| f.diff.as_str()).filter(|d| !d.is_empty()).collect();
                if !diffs.is_empty() {
                    out.display = Some(diffs.join("\n"));
                }
                out.changes = writer.changes.into_inner().unwrap_or_default();
                out
            }
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn near_miss_headers_are_found_and_real_ones_are_not() {
        let ok = "*** Edit File: a.rs\n*** Find\nx\n*** Replace\ny\n*** Edit File: b.rs all\n*** Insert Before\nz\n\
                  *** Insert After\n***bold*** text\n*** Finding things\n*** End Patch\n    *** Find all\n";
        assert_eq!(near_miss_header(ok), None);
        for (bad, line) in [
            ("*** Find\nx\n*** Find all\n", 3),
            ("*** all\n", 1),
            ("***Find\n", 1),
            ("*** Replace now\n", 1),
            ("*** Insert\n", 1),
            ("***Edit File: a.rs\n", 1),
            ("*** Insert After here\n", 1),
        ] {
            assert_eq!(near_miss_header(bad).map(|(n, _)| n), Some(line), "{bad:?}");
        }
    }

    #[test]
    fn a_near_miss_is_refused_in_sloppy_mode_only() {
        let input = "*** Edit File: x.txt\n*** Find\na\n*** Replace\nb\n*** Find all\n";
        let r = refuse_near_miss(EditMode::Sloppy, input).unwrap();
        assert!(r.is_error && r.text.contains("Line 6") && r.text.contains("indent it"), "{}", r.text);
        assert!(refuse_near_miss(EditMode::Hashline, input).is_none());
    }
}
