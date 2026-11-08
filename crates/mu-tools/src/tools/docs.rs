//! `mu://`: mu's own documentation. The pages are embedded in the binary, so they match the running
//! version, and are written to disk on first use, so `read`, `grep` and `glob` work on real files.
//!
//! Three kinds of page: the hand-written docs, reference pages generated from the code (the tools
//! here; front ends add theirs with `register`), and `mu://state/`, which describes this session and
//! is rewritten each time it is read.

use super::{Tools, uri};
use mu_common::paths;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const PAGES: &[(&str, &str)] = &[
    ("overview.md", include_str!("../../../../README.md")),
    ("design.md", include_str!("../../../../DESIGN.md")),
    ("acp.md", include_str!("../../../../docs/acp.md")),
    ("background-jobs.md", include_str!("../../../../docs/background-jobs.md")),
    ("config.md", include_str!("../../../../docs/config.md")),
    ("custom-agents.md", include_str!("../../../../docs/custom-agents.md")),
    ("detached-runs.md", include_str!("../../../../docs/detached-runs.md")),
    ("durable-plans.md", include_str!("../../../../docs/durable-plans.md")),
    ("ghost-text.md", include_str!("../../../../docs/ghost-text.md")),
    ("mcp.md", include_str!("../../../../docs/mcp.md")),
    ("memory.md", include_str!("../../../../docs/memory.md")),
    ("model-roles.md", include_str!("../../../../docs/model-roles.md")),
    ("permissions.md", include_str!("../../../../docs/permissions.md")),
    ("providers.md", include_str!("../../../../docs/providers.md")),
    ("reply-picker.md", include_str!("../../../../docs/reply-picker.md")),
    ("review.md", include_str!("../../../../docs/review.md")),
    ("secrets.md", include_str!("../../../../docs/secrets.md")),
    ("self-documentation.md", include_str!("../../../../docs/self-documentation.md")),
    ("usage.md", include_str!("../../../../docs/usage.md")),
    ("web.md", include_str!("../../../../docs/web.md")),
];

/// The docs index (`docs/README.md`) as embedded, up to the line that points outside the docs.
const INDEX: &str = include_str!("../../../../docs/README.md");

type Page = fn() -> String;

fn registry() -> &'static Mutex<BTreeMap<&'static str, Page>> {
    static R: OnceLock<Mutex<BTreeMap<&'static str, Page>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Adds a generated page at `path` (relative, e.g. `reference/cli.md`) for front ends to describe
/// themselves. Call before the first `mu://` read.
pub fn register(path: &'static str, page: Page) {
    registry().lock().unwrap_or_else(|e| e.into_inner()).insert(path, page);
}

static VERSION: OnceLock<&'static str> = OnceLock::new();

/// The version string shown on the index and in `mu://state/session.md`.
pub fn set_version(v: &'static str) {
    let _ = VERSION.set(v);
}

fn version() -> &'static str {
    VERSION.get().copied().unwrap_or("unknown")
}

fn index() -> String {
    let (head, _) = INDEX.split_once("\nDesign and measurements:").unwrap_or((INDEX, ""));
    let head = head.replacen("# mu docs", &format!("# mu docs (mu {})", version()), 1);
    let mut s = format!(
        "{head}\n- [Overview](overview.md): what mu is and how to install it\n- [Design](design.md): principles, crates and layers, plugins, providers, compatibility\n\n\
These pages ship inside the running mu. Read one with `read mu://<page>`; search all of them with `grep` or `glob` on `mu://`.\n\n\
## Reference, generated from the running binary\n\n"
    );
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let generated = std::iter::once("reference/tools.md").chain(reg.keys().copied());
    let mut generated: Vec<&str> = generated.collect();
    generated.sort_unstable();
    generated.dedup();
    for p in generated {
        s.push_str(&format!("- [{p}]({p})\n"));
    }
    s.push_str(
        "\n## This session, `mu://state/`\n\nRewritten each time it is read, and not part of a search of `mu://`: search `mu://state` itself.\n\n\
- [state/session.md](state/session.md): version, directories, ids and files\n\
- [state/config.md](state/config.md): the config files in effect\n\
- [state/plugins.md](state/plugins.md): plugins and MCP servers, and their tools\n\
- [state/context.md](state/context.md): context files, skills, commands and custom agents loaded\n",
    );
    s
}

fn pages() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = PAGES.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect();
    v.push(("README.md".into(), index()));
    v.push(("reference/tools.md".into(), tools_reference()));
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    v.extend(reg.iter().map(|(p, f)| (p.to_string(), f())));
    v
}

fn tools_reference() -> String {
    let mut defs = Tools::definitions();
    defs.push(Tools::task_def(true, false, &[], &[]));
    defs.push(Tools::job_cancel_def());
    defs.push(Tools::wait_def());
    defs.push(Tools::plan_def());
    let mut s = String::from(
        "# Tools\n\nThe built-in tools the model is offered. Plugin and MCP tools are listed in `mu://state/plugins.md`.\n",
    );
    for d in defs {
        s.push_str(&format!("\n## {}\n\n{}\n", d.name, d.description.trim()));
        let Some(props) = d.schema.get("properties").and_then(|p| p.as_object()) else { continue };
        let required: Vec<&str> = d
            .schema
            .get("required")
            .and_then(|r| r.as_array())
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str())
            .collect();
        s.push_str("\nParameters:\n");
        for (name, p) in props {
            let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("any");
            let req = if required.contains(&name.as_str()) { ", required" } else { "" };
            let desc = p.get("description").and_then(|t| t.as_str()).map(|t| format!(": {t}")).unwrap_or_default();
            s.push_str(&format!("- `{name}` ({ty}{req}){desc}\n"));
        }
    }
    s
}

fn stamp(pages: &[(String, String)]) -> String {
    let mut h = Sha256::new();
    for (p, c) in pages {
        h.update(p.as_bytes());
        h.update([0]);
        h.update(c.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn write_page(root: &Path, rel: &str, content: &str) -> io::Result<()> {
    let p = root.join(rel);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, content)
}

/// The shared tree of static pages, `<mu home>/docs/<stamp>/`, written once per distinct content.
/// A directory left by another version is removed; a missing one is written again.
fn static_root() -> io::Result<PathBuf> {
    static ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);
    let mut cached = ROOT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = cached.as_ref().filter(|p| p.join(".complete").exists()) {
        return Ok(p.clone());
    }
    let pages = pages();
    let base = paths::mu_home().join("docs");
    let name = stamp(&pages);
    let dir = base.join(&name);
    if !dir.join(".complete").exists() {
        let tmp = base.join(format!(".{name}.tmp{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for (p, c) in &pages {
            write_page(&tmp, p, c)?;
        }
        std::fs::create_dir_all(tmp.join("state"))?;
        std::fs::write(tmp.join(".complete"), "")?;
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = std::fs::rename(&tmp, &dir) {
            let _ = std::fs::remove_dir_all(&tmp);
            // Another mu wrote the same tree first.
            if !dir.join(".complete").exists() {
                return Err(e);
            }
        }
        for e in std::fs::read_dir(&base)?.flatten() {
            let n = e.file_name();
            if n.to_str().is_some_and(|n| n != name && !n.starts_with('.')) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    *cached = Some(dir.clone());
    Ok(dir)
}

fn fence(lang: &str, text: &str) -> String {
    format!("```{lang}\n{}\n```\n", text.trim_end())
}

fn file_section(s: &mut String, path: &Path) {
    s.push_str(&format!("\n## {}\n\n", path.display()));
    match std::fs::read_to_string(path) {
        Ok(t) if !t.trim().is_empty() => s.push_str(&fence("toml", &t)),
        Ok(_) => s.push_str("Empty: every default applies.\n"),
        Err(_) => s.push_str("Not present: every default applies.\n"),
    }
}

impl Tools {
    /// Free text for `mu://state/context.md`: what context discovery loaded.
    pub fn set_context_page(&self, text: String) {
        *self.session.context_page.borrow_mut() = text;
    }

    fn state_pages(&self) -> Vec<(&'static str, String)> {
        let id = self.session.id.borrow().clone();
        let file = self.session.path.borrow().as_ref().map(|p| p.display().to_string());
        let session = format!(
            "# This session\n\n- mu: {}\n- working directory: {}\n- profile: {}\n- mu home: {}\n- profile home (sessions, plans, models.toml): {}\n- session id: {}\n- session file: {}\n- `local://` is {}\n- `artifact://` is {}\n- `mu://` docs are written to {}\n",
            version(),
            self.cwd.display(),
            paths::safe_profile().unwrap_or_else(|| "none".into()),
            paths::mu_home().display(),
            paths::profile_home().display(),
            if id.is_empty() { "none" } else { &id },
            file.as_deref().unwrap_or("none"),
            self.local_root().display(),
            self.artifact_dir().display(),
            paths::mu_home().join("docs").display(),
        );
        let mut config = String::from(
            "# Config in effect\n\nKeys that are absent take their defaults; see `mu://config.md` and `mu://model-roles.md`.\n",
        );
        file_section(&mut config, &paths::mu_home().join("config.toml"));
        file_section(&mut config, &mu_base::config::models_path());
        let mut plugins = String::from("# Plugins and MCP servers\n\n");
        let status = self.plugins.status();
        if status.is_empty() {
            plugins.push_str("None.\n");
        }
        for (name, label, running, extra) in status {
            let state = if running { "running" } else { "idle" };
            let extra = if extra.is_empty() { String::new() } else { format!(" · {extra}") };
            plugins.push_str(&format!("- {name}: {label} · {state}{extra}\n"));
        }
        plugins.push_str("\n## Tools they provide\n\n");
        let defs = self.plugins.tool_defs();
        if defs.is_empty() {
            plugins.push_str("None.\n");
        }
        for d in defs {
            let first = d.description.lines().next().unwrap_or("");
            plugins.push_str(&format!("- `{}`: {first}\n", d.name));
        }
        let context = {
            let c = self.session.context_page.borrow();
            if c.is_empty() { "# Context\n\nNot loaded.\n".to_string() } else { c.clone() }
        };
        vec![("session.md", session), ("config.md", config), ("plugins.md", plugins), ("context.md", context)]
    }

    /// The directory behind `mu://state/`, brought up to date.
    pub(super) fn refresh_state(&self) -> io::Result<PathBuf> {
        let dir = uri::state_dir(&self.session.id.borrow());
        std::fs::create_dir_all(&dir)?;
        for (name, content) in self.state_pages() {
            if std::fs::read_to_string(dir.join(name)).ok().as_deref() != Some(&content) {
                std::fs::write(dir.join(name), content)?;
            }
        }
        Ok(dir)
    }

    /// The directory behind a `mu://` path: `state/…` is this session's, the rest the shared tree.
    pub(super) fn mu_path(&self, rel: &str) -> Result<PathBuf, String> {
        let rel = rel.trim_start_matches('/');
        let unavailable = |e: io::Error| format!("mu:// docs are unavailable: {e}");
        match rel.strip_prefix("state") {
            Some(r) if r.is_empty() || r.starts_with('/') => {
                uri::safe_join(&self.refresh_state().map_err(unavailable)?, r)
            }
            _ => uri::safe_join(&static_root().map_err(unavailable)?, rel),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_doc_is_embedded() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".md") && n != "README.md" {
                assert!(PAGES.iter().any(|(p, _)| *p == n), "docs/{n} is not embedded in tools/docs.rs");
            }
        }
    }

    #[test]
    fn index_lists_tools_and_state() {
        let i = index();
        assert!(i.contains("](usage.md)") && !i.contains("](../DESIGN.md)"), "{i}");
        assert!(i.contains("reference/tools.md") && i.contains("state/session.md"));
    }

    #[test]
    fn tools_reference_names_parameters() {
        let t = tools_reference();
        assert!(t.contains("## read") && t.contains("- `path` (string, required)"), "{t}");
    }

    #[test]
    fn static_tree_is_written_once_and_listed() {
        mu_testutil::home();
        let a = static_root().unwrap();
        let b = static_root().unwrap();
        assert_eq!(a, b);
        assert!(a.join("README.md").is_file() && a.join("reference/tools.md").is_file() && a.join("state").is_dir());
        assert!(a.join("usage.md").is_file());
    }

    fn args<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
        serde_json::from_value(v).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_grep_and_glob_see_mu_urls() {
        mu_testutil::home();
        let t = Tools::for_test();
        t.set_session("sid", None);
        let listing = t.read(args(serde_json::json!({"path": "mu://"}))).await;
        assert!(
            !listing.is_error && listing.text.contains("usage.md") && listing.text.contains("reference"),
            "{}",
            listing.text
        );
        let page = t.read(args(serde_json::json!({"path": "mu://usage.md:1-3"}))).await;
        assert!(!page.is_error && page.text.contains("1:"), "{}", page.text);
        let hits = t.grep(args(serde_json::json!({"pattern": "propose_plan", "path": "mu://"}))).await;
        assert!(hits.text.contains("mu://reference/tools.md:"), "rows name the URL: {}", hits.text);
        let files = t.glob(args(serde_json::json!({"pattern": "reference/*.md", "path": "mu://"}))).await;
        assert!(files.text.contains("mu://reference/tools.md"), "{}", files.text);
        let state = t.grep(args(serde_json::json!({"pattern": "session id", "path": "mu://state"}))).await;
        assert!(state.text.contains("mu://state/session.md:"), "{}", state.text);
        let missing = t.read(args(serde_json::json!({"path": "mu://nope.md"}))).await;
        assert!(missing.is_error, "{}", missing.text);
        let w = t.write(args(serde_json::json!({"path": "mu://usage.md", "content": "x"})), false).await;
        assert!(w.is_error && w.text.contains("read-only"), "{}", w.text);
    }

    #[test]
    fn state_pages_describe_the_session() {
        let t = Tools::for_test();
        t.set_session("sid", None);
        t.set_context_page("# Context\n\n- skill: x\n".into());
        let p = t.mu_path("state/session.md").unwrap();
        let s = std::fs::read_to_string(p).unwrap();
        assert!(s.contains("session id: sid") && s.contains("working directory: /tmp"), "{s}");
        assert!(std::fs::read_to_string(t.mu_path("state/context.md").unwrap()).unwrap().contains("skill: x"));
        assert!(t.mu_path("state/../x").is_err());
        assert!(t.mu_path("statement.md").unwrap().ends_with("statement.md"), "only `state` is the session's");
    }
}
