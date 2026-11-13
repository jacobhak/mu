//! `memory/*` methods the TUI's `/memory` talks to.

use crate::mem::{Config, Mem};
use crate::{ctx, gitx, review, store};
use serde_json::{Value, json};

pub async fn call(method: &str, p: &Value) -> Result<Value, String> {
    let c = ctx::get();
    let mem = Mem::open(&c.cwd, Config::load()).await;
    match method {
        "memory/status" => status(&mem),
        "memory/files" => files(&mem),
        "memory/log" => {
            let n = p["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
            let history = mem.root.join(".git").exists();
            let commits = gitx::log(&mem.root, n).await?;
            Ok(json!({ "history": history, "commits": commits }))
        }
        "memory/show" => Ok(json!({ "text": gitx::show(&mem.root, p["sha"].as_str().unwrap_or_default()).await? })),
        "memory/revert" => {
            let sha = p["sha"].as_str().unwrap_or_default();
            let _g = mem.lock().await?;
            if !mem.prepare().await {
                return Err("git is unavailable, so there is no history to revert".into());
            }
            gitx::revert(&mem.root, sha).await?;
            Ok(json!({ "ok": true }))
        }
        other => Err(format!("unknown method {other}")),
    }
}

fn status(mem: &Mem) -> Result<Value, String> {
    let state = mem.load()?;
    let notes: Vec<Value> = state
        .notes
        .iter()
        .map(|(slug, n)| json!({ "slug": slug, "path": mem.note_path(slug), "description": n.description, "type": n.kind.name() }))
        .collect();
    Ok(json!({
        "root": mem.root,
        "checkout": mem.checkout,
        "history": mem.root.join(".git").exists(),
        "review": mem.cfg.review,
        "files": [
            { "label": "User", "path": mem.user_path(), "size": store::size_of(&state.user), "cap": mem.cfg.caps.user },
            { "label": "This repository", "path": mem.project_path(), "size": store::size_of(&state.project), "cap": mem.cfg.caps.project },
        ],
        "notes": notes,
        "queue": review::queue_len(),
    }))
}

/// The paths the picker opens. The two entry files are created empty when missing so an editor can open them.
fn files(mem: &Mem) -> Result<Value, String> {
    for p in [mem.user_path(), mem.project_path()] {
        if !p.exists() {
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
            std::fs::write(&p, "").map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    status(mem)
}
