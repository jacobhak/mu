//! The `memory` tool: `read` and `edit`.

use crate::ctx::Ctx;
use crate::mem::{Config, Mem};
use crate::snapshot;
use crate::store::{self, File, Op};
use serde_json::Value;

pub async fn call(args: &Value, ctx: &Ctx) -> (String, bool) {
    let mem = Mem::open(&ctx.cwd, Config::load()).await;
    let r = match args["action"].as_str() {
        Some("read") => read(&mem, args["note"].as_str()),
        Some("edit") => edit(&mem, args, ctx).await,
        _ => Err("action must be `read` or `edit`".into()),
    };
    match r {
        Ok(t) => (t, false),
        Err(e) => (e, true),
    }
}

fn read(mem: &Mem, note: Option<&str>) -> Result<String, String> {
    let state = mem.load();
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(slug) => match state.notes.get(slug) {
            Some(n) => Ok(format!("# {slug} ({})\n{}\n\n{}", n.kind.name(), n.description, n.body)),
            None => {
                let names: Vec<&str> = state.notes.keys().map(String::as_str).collect();
                Err(format!(
                    "no note {slug:?}. Notes: {}",
                    if names.is_empty() { "(none)".into() } else { names.join(", ") }
                ))
            }
        },
        None => Ok(snapshot::live(&state, &mem.checkout, &mem.cfg.caps)),
    }
}

async fn edit(mem: &Mem, args: &Value, ctx: &Ctx) -> Result<String, String> {
    let ops: Vec<Op> = serde_json::from_value(args["ops"].clone())
        .map_err(|e| format!("ops must be an array of add/replace/remove/write_note/delete_note ops: {e}"))?;
    if ops.is_empty() {
        return Err("ops is empty".into());
    }
    let e = mem.edit(ops, &ctx.cwd, &format!("tool {}", ctx.session_id)).await?;
    let mut out = String::new();
    if !e.outcome.changed() {
        out.push_str("No change: everything was already saved.\n");
    } else {
        out.push_str("Saved.\n");
    }
    for f in [File::User, File::Project] {
        if e.outcome.files.contains(&f) {
            out.push_str(&format!(
                "\n{} ({}/{} characters):\n{}",
                f.name(),
                store::size_of(e.state.entries(f)),
                mem.cfg.caps.of(f),
                list(e.state.entries(f))
            ));
        }
    }
    for n in &e.outcome.notes {
        match e.state.notes.get(n) {
            Some(note) => out.push_str(&format!("\nnote {n} written: {}\n", note.description)),
            None => out.push_str(&format!("\nnote {n} deleted\n")),
        }
    }
    if !e.redacted.is_empty() {
        out.push_str(&format!(
            "\n[memory: secrets were replaced before saving: {}. Never put secret values in memory.]\n",
            e.redacted.join(", ")
        ));
    }
    if !e.history {
        out.push_str("\n[memory: git is unavailable, so this change has no history.]\n");
    }
    Ok(out)
}

fn list(entries: &[String]) -> String {
    if entries.is_empty() {
        return "(no entries)\n".into();
    }
    entries.iter().map(|e| format!("- {e}\n")).collect()
}
