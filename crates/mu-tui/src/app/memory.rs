//! `/memory`: the memory files and notes the `memory` plugin keeps, its commit history, and the
//! diff and revert of a commit. Every request goes to the host, which asks the plugin.

use super::child::ChildThen;
use super::*;
use crate::host::memory::{PLUGIN_REQUEST, PluginRequest};
use crate::widgets::overlay::MemoryPick;
use crate::widgets::picker::Picker;
use crate::widgets::select::{Item, Select};
use serde_json::Value;

/// Git and the plugin's writes can take a moment; a request that runs longer is reported.
const TIMEOUT: Duration = Duration::from_secs(10);

const LOG_LIMIT: u64 = 50;

const MEMORY_HINT: &str = "↑↓ move · Enter open · Esc close";
const HISTORY_HINT: &str = "↑↓ move · Enter show diff · r revert · Esc close";

impl App {
    /// `/memory`: opens the picker on a loading row, then fills it with the files and notes.
    pub(super) fn open_memory(&mut self) {
        let items = vec![Item { label: "Loading…".into(), detail: String::new() }];
        let pick = Picker::placeholder(items, |items| Select::new("Memory", items));
        self.open_overlay(Overlay::Memory { pick });
        self.memory_request("memory/files", json!({ "cwd": self.cwd }), |app, r| app.show_memory(r));
    }

    /// Fills the picker that `open_memory` opened. A reply that comes after the picker was closed
    /// is dropped, but its failure is still said.
    pub(super) fn show_memory(&mut self, r: Result<Value, String>) {
        let loading = matches!(&self.overlay, Some(Overlay::Memory { pick }) if pick.is_empty());
        match r {
            Ok(v) if loading => {
                let (rows, context) = memory_rows(&v);
                let pick =
                    Picker::from_rows(rows, |items| Select::new("Memory", items).context(&context).hint(MEMORY_HINT));
                self.open_overlay(Overlay::Memory { pick });
            }
            Ok(_) => {}
            Err(e) => {
                if loading {
                    self.overlay = None;
                }
                self.notice(&format!("memory: {e}"));
            }
        }
    }

    /// The `memory/log` answer: the commits as a picker, or a notice saying why there are none.
    pub(super) fn show_memory_log(&mut self, r: Result<Value, String>) {
        let v = match r {
            Ok(v) => v,
            Err(e) => return self.notice(&format!("memory: {e}")),
        };
        if !v["history"].as_bool().unwrap_or(false) {
            return self.notice("no memory history yet: nothing has been written, or git is missing");
        }
        let rows = memory_commits(&v);
        if rows.is_empty() {
            return self.notice("no memory history yet");
        }
        // A list only replaces the picker or nothing: another overlay the user opened stays.
        if !matches!(self.overlay, None | Some(Overlay::Memory { .. }) | Some(Overlay::MemoryLog { .. })) {
            return;
        }
        let title = format!("Memory history ({} commits)", rows.len());
        let pick = Picker::from_rows(rows, |items| Select::new(&title, items).hint(HISTORY_HINT));
        self.open_overlay(Overlay::MemoryLog { pick });
    }

    /// Opens a memory file in `$VISUAL` or `$EDITOR`. Without either the path is said instead.
    pub(super) fn open_memory_file(&mut self, path: PathBuf) {
        let set = |name: &str| std::env::var(name).is_ok_and(|v| !v.trim().is_empty());
        if !set("VISUAL") && !set("EDITOR") {
            return self.notice(&format!("no $EDITOR set; the memory file is {}", path.display()));
        }
        self.edit_host_file(path, true, "memory file", ChildThen::MemoryEdit);
    }

    pub(super) fn open_memory_history(&mut self) {
        self.memory_request("memory/log", json!({ "limit": LOG_LIMIT }), |app, r| app.show_memory_log(r));
    }

    pub(super) fn show_memory_commit(&mut self, sha: String) {
        self.memory_request("memory/show", json!({ "sha": sha }), |app, r| app.on_memory_shown(r));
    }

    pub(super) fn revert_memory_commit(&mut self, sha: String) {
        let short = short(&sha).to_string();
        self.memory_request("memory/revert", json!({ "sha": sha }), move |app, r| app.on_memory_reverted(&short, r));
    }

    /// The commit's message, stat and diff, as a notice in the transcript.
    pub(super) fn on_memory_shown(&mut self, r: Result<Value, String>) {
        match r {
            Ok(v) => self.notice(&text(&v, "text")),
            Err(e) => self.notice(&format!("memory: {e}")),
        }
    }

    /// After a revert the history is read again, so the picker shows the revert commit.
    pub(super) fn on_memory_reverted(&mut self, short: &str, r: Result<Value, String>) {
        match r {
            Ok(_) => {
                self.notice(&format!("reverted commit {short}"));
                self.open_memory_history();
            }
            Err(e) => self.notice(&format!("memory: could not revert {short}: {e}")),
        }
    }

    fn memory_request(
        &mut self,
        method: &'static str,
        params: Value,
        then: impl FnOnce(&mut App, Result<Value, String>) + 'static,
    ) {
        let args =
            PluginRequest { plugin: "memory".into(), method: method.into(), params, timeout_secs: TIMEOUT.as_secs() };
        self.host_call(PLUGIN_REQUEST, &args, then);
    }
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The picker's rows from `memory/files`: the two entry files, the notes, then the history; and
/// the line above them.
fn memory_rows(v: &Value) -> (Vec<(Item, MemoryPick)>, String) {
    let mut rows = Vec::new();
    for f in v["files"].as_array().into_iter().flatten() {
        let (size, cap) = (f["size"].as_u64().unwrap_or(0), f["cap"].as_u64().unwrap_or(0));
        let item = Item { label: text(f, "label"), detail: format!("{size}/{cap}") };
        rows.push((item, MemoryPick::File(text(f, "path").into())));
    }
    for n in v["notes"].as_array().into_iter().flatten() {
        let item = Item { label: text(n, "slug"), detail: text(n, "description") };
        rows.push((item, MemoryPick::File(text(n, "path").into())));
    }
    let history = v["history"].as_bool().unwrap_or(false);
    let detail = if history { format!("last {LOG_LIMIT} commits") } else { "no commits yet".into() };
    rows.push((Item { label: "History".into(), detail }, MemoryPick::History));
    let review = if v["review"].as_bool() == Some(true) { "on" } else { "off" };
    let context = format!("review {review} · {} queued", v["queue"].as_u64().unwrap_or(0));
    (rows, context)
}

/// The history's rows from `memory/log`, newest first, with the commit ids they stand for.
fn memory_commits(v: &Value) -> Vec<(Item, String)> {
    v["commits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let sha = text(c, "sha");
            let message = text(c, "message");
            let first = message.lines().next().unwrap_or_default();
            let files: Vec<&str> = c["files"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let item = Item {
                label: format!("{} {}", short(&sha), trunc(first, 70)),
                detail: format!("{} · {}", text(c, "time"), trunc(&files.join(", "), 60)),
            };
            (item, sha)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::keys::tests::{in_local, test_app_with_commands, transcript_text};
    use crate::host::{Deferred, local::LocalHost};

    /// Runs `ask` on an app whose host is the local one, then on one wrapped in `Deferred`, pumping
    /// the replies; returns what each transcript says.
    fn both_hosts(ask: impl Fn(&mut App)) -> (String, String) {
        let mut said = Vec::new();
        for deferred in [false, true] {
            let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-memory-test"));
            let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
            app.ui_tx = ui_tx;
            if deferred {
                app.host = Rc::new(Deferred(LocalHost::new(
                    app.services.providers.clone(),
                    app.ctx.clone(),
                    app.plugins.clone(),
                    app.cwd.clone(),
                )));
            }
            in_local(async {
                ask(&mut app);
                let m = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv()).await.unwrap().unwrap();
                app.on_ui(m);
            });
            said.push(transcript_text(&app));
        }
        (said.remove(0), said.remove(0))
    }

    #[test]
    fn a_memory_request_says_why_it_failed_the_same_with_a_deferred_host() {
        let (now, later) = both_hosts(|app| app.show_memory_commit("0123456789".into()));
        assert!(now.contains("memory: no plugin memory"), "{now}");
        assert_eq!(now, later);
    }

    #[test]
    fn the_log_and_the_revert_report_the_same_with_a_deferred_host() {
        let (now, later) = both_hosts(|app| app.revert_memory_commit("0123456789abc".into()));
        assert!(now.contains("memory: could not revert 0123456: no plugin memory"), "{now}");
        assert_eq!(now, later);
        let (now, later) = both_hosts(|app| app.open_memory_history());
        assert!(now.contains("memory: no plugin memory"), "{now}");
        assert_eq!(now, later);
    }

    #[test]
    fn a_non_memory_plugin_is_refused_by_the_host() {
        let (now, later) = both_hosts(|app| {
            let args =
                PluginRequest { plugin: "predict".into(), method: "x".into(), params: json!({}), timeout_secs: 1 };
            app.host_call(PLUGIN_REQUEST, &args, |app, r: Result<Value, String>| app.notice(&r.unwrap_err()));
        });
        assert!(now.contains("memory plugin only, not predict"), "{now}");
        assert_eq!(now, later);
    }

    #[test]
    fn opening_the_picker_closes_it_again_when_the_files_cannot_be_read() {
        let (now, later) = both_hosts(|app| app.open_memory());
        assert!(now.contains("memory: no plugin memory"), "{now}");
        assert_eq!(now, later);
    }

    #[test]
    fn picker_rows_follow_the_plugin_status() {
        let v = json!({
            "history": true,
            "review": false,
            "queue": 2,
            "files": [
                {"label": "User", "path": "/m/user.md", "size": 120, "cap": 4000},
                {"label": "This repository", "path": "/m/p.md", "size": 0, "cap": 8000},
            ],
            "notes": [{"slug": "rust", "path": "/m/notes/rust.md", "description": "Build notes", "type": "note"}],
        });
        let (rows, context) = memory_rows(&v);
        let (items, picks): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["User", "This repository", "rust", "History"]);
        assert_eq!(items[0].detail, "120/4000");
        assert_eq!(items[2].detail, "Build notes");
        assert_eq!(
            picks,
            [
                MemoryPick::File("/m/user.md".into()),
                MemoryPick::File("/m/p.md".into()),
                MemoryPick::File("/m/notes/rust.md".into()),
                MemoryPick::History,
            ]
        );
        assert_eq!(context, "review off · 2 queued");
    }

    #[test]
    fn history_without_git_says_so_in_its_row() {
        let (rows, _) = memory_rows(&json!({"history": false, "files": [], "notes": []}));
        assert_eq!(rows[0].0.detail, "no commits yet");
    }

    #[test]
    fn commits_are_labelled_by_their_first_line() {
        let v = json!({"history": true, "commits": [
            {"sha": "0123456789abcdef", "time": "2026-11-06 10:00", "message": "manual edit\n\nbody", "files": ["user.md", "notes/rust.md"]}
        ]});
        let rows = memory_commits(&v);
        assert_eq!(rows[0].0.label, "0123456 manual edit");
        assert_eq!(rows[0].0.detail, "2026-11-06 10:00 · user.md, notes/rust.md");
        assert_eq!(rows[0].1, "0123456789abcdef");
    }
}
