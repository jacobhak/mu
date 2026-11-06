//! The `session_search` tool: an FTS5 index over the profile's session files, kept up to date on each call.
//! The index lives in `<profile home>/session-index.db`; see `index` for what is stored.

mod index;
mod query;
#[cfg(test)]
mod tests;

use crate::ctx::Ctx;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Characters shown per message.
const MSG_CHARS: usize = 400;
/// A first index run longer than this is announced in the result.
const NOTE_AFTER: Duration = Duration::from_secs(2);

pub async fn call(args: &Value, ctx: &Ctx) -> (String, bool) {
    let query = args["query"].as_str().unwrap_or_default().trim().to_string();
    if query.is_empty() {
        return ("session_search needs a non-empty `query`.".into(), true);
    }
    let repo_scope = match args["scope"].as_str().unwrap_or("repo") {
        "repo" => true,
        "all" => false,
        other => return (format!("scope must be \"repo\" or \"all\", not {other:?}."), true),
    };
    let limit = args["limit"].as_u64().unwrap_or(5).clamp(1, 10) as usize;
    match run(&query, repo_scope, limit, ctx).await {
        Ok(text) => (text, false),
        Err(e) => (format!("session_search failed: {e}"), true),
    }
}

async fn run(q: &str, repo_scope: bool, limit: usize, ctx: &Ctx) -> Result<String, String> {
    let repo = if repo_scope { Some(repo_key(&ctx.cwd).await) } else { None };
    let home = mu_common::paths::profile_home();
    let (dir, db) = (home.join("sessions"), home.join(index::DB_FILE));
    let started = Instant::now();
    let mut keys: HashMap<PathBuf, String> = HashMap::new();
    let mut indexed = 0;
    loop {
        let (d, b, k) = (dir.clone(), db.clone(), keys.clone());
        let rep = blocking(move || index::update(&d, &b, &k)).await?;
        indexed += rep.indexed;
        if rep.needed.is_empty() {
            break;
        }
        for cwd in rep.needed {
            let key = repo_key(&cwd).await;
            keys.insert(cwd, key);
        }
    }
    let took = started.elapsed();
    let note = (indexed > 5 && took > NOTE_AFTER)
        .then(|| format!("Indexed {indexed} sessions in {:.1} s.", took.as_secs_f32()));
    let excl = ctx.session_id.clone();
    let q_owned = q.to_string();
    let found = blocking(move || query::run(&db, &q_owned, repo.as_deref(), &excl, limit)).await?;
    Ok(render(q, repo_scope, &found, note))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

/// The repo key of `cwd`'s clone. A directory that no longer exists keys by its own path.
async fn repo_key(cwd: &Path) -> String {
    if !cwd.exists() {
        return mu_common::paths::encode_cwd(cwd);
    }
    mu_base::git::repo_key(cwd).await
}

fn render(q: &str, repo_scope: bool, found: &[query::Session], note: Option<String>) -> String {
    let mut out = String::new();
    if let Some(n) = note {
        out += &n;
        out += "\n\n";
    }
    if found.is_empty() {
        let scope = if repo_scope { " in this repository (scope \"all\" searches every repository)" } else { "" };
        out += &format!("No past session mentions {q:?}{scope}.");
        return out;
    }
    for (i, s) in found.iter().enumerate() {
        let title = s.title.as_deref().map(|t| clip(t, 200)).filter(|t| !t.is_empty());
        let title = title.unwrap_or_else(|| "(untitled)".into());
        let date: String = s.started.chars().take(10).collect();
        out += &format!("{}. {title} — {date} — {}\n", i + 1, s.path);
        for hit in &s.hits {
            for l in hit {
                let mark = if l.hit { ">" } else { " " };
                out += &format!("   {mark} {}: {}\n", l.role, clip(&l.text, MSG_CHARS));
            }
        }
        out += "\n";
    }
    out.trim_end().to_string()
}

/// `s` with whitespace collapsed, cut to `max` characters (with `…` when cut).
fn clip(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}
