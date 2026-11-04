use super::Tools;
use super::uri::Scheme;
use async_trait::async_trait;
use mu_base::tool::{FileMeta, ToolMeta, ToolOutput};
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

struct FsWriter;

fn werr(e: impl std::fmt::Display) -> EditError {
    EditError::Writer(e.to_string())
}

#[async_trait]
impl EditWriter for FsWriter {
    async fn write(&self, r: WriteRequest) -> EditResult<WriteResponse> {
        match r.op {
            FileOp::Delete => {
                tokio::fs::remove_file(&r.absolute).await.map_err(werr)?;
                Ok(WriteResponse::default())
            }
            FileOp::Noop => Ok(WriteResponse::default()),
            FileOp::Create | FileOp::Update => {
                let content = r.content.ok_or_else(|| werr("edit request omitted content"))?;
                let dest = r.move_to.as_deref().unwrap_or(&r.absolute);
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
                Ok(WriteResponse { written: content, ..Default::default() })
            }
        }
    }
}

impl Tools {
    pub async fn edit(&self, a: Args) -> ToolOutput {
        let policy = PathPolicy {
            cwd: self.cwd.clone(),
            home_dir: std::env::var_os("HOME").map(Into::into).unwrap_or_default(),
            url_schemes: vec!["local".into(), "artifact".into(), "skill".into()],
            url_alias_schemes: vec![],
            plan_writable_roots: vec![self.local_root()],
            plan_active: self.plan.get(),
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
        match s.apply(ApplyRequest::default(), &FsWriter).await {
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
                out
            }
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}
