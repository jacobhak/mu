//! `/modelset`: switch, create, delete and rename model sets, and copy omp's model config into
//! `models.toml` (also offered once at startup).

use super::*;
use crate::host::models::{self as models_host, Imported, Rename, SetTable, Switched};
use crate::widgets::overlay::Overlay;
use crate::widgets::picker::Picker;
use crate::widgets::select::{Item, Select};

const USAGE: &str = "usage: /modelset [name | new <name> | delete <name> | rename <old> <new> | import]";

impl App {
    pub(super) fn modelset_cmd(&mut self, arg: &str) {
        let words: Vec<&str> = arg.split_whitespace().collect();
        match words.as_slice() {
            [] => self.open_set_picker(),
            ["import"] => self.import_now(),
            [word] if !matches!(*word, "new" | "delete" | "rename") => self.switch_to(word),
            ["new", name] => {
                let name = name.to_string();
                self.host_call(models_host::NEW_SET, &name.clone(), move |app, r: Result<Option<String>, String>| {
                    app.set_changed(r.map(|from| {
                        format!(
                            "model set {name} created from {} and active; /model edits its roles",
                            from.as_deref().unwrap_or("nothing")
                        )
                    }))
                });
            }
            ["delete", name] => {
                let name = name.to_string();
                self.host_call(models_host::DELETE_SET, &name.clone(), move |app, r: Result<(), String>| {
                    app.set_changed(r.map(|()| format!("model set {name} deleted")))
                });
            }
            ["rename", old, new] => {
                let args = Rename { old: old.to_string(), new: new.to_string() };
                self.host_call(models_host::RENAME_SET, &args.clone(), move |app, r: Result<(), String>| {
                    app.set_changed(r.map(|()| format!("model set {} renamed to {}", args.old, args.new)))
                });
            }
            _ => self.error(USAGE),
        }
    }

    /// A set was created, deleted or renamed: the completion list names the sets.
    fn set_changed(&mut self, result: Result<String, String>) {
        match result {
            Ok(msg) => {
                self.refresh_commands();
                self.notice(&msg);
            }
            Err(e) => self.error(&e),
        }
    }

    pub(super) fn open_set_picker(&mut self) {
        self.host_call(models_host::SET_TABLE, &(), |app, r: Result<SetTable, String>| {
            let table = match r {
                Ok(t) => t,
                Err(e) => return app.error(&format!("cannot read the model sets: {e}")),
            };
            if table.sets.is_empty() {
                let mut msg =
                    "no model sets yet: roles you save with /model go into set main; /modelset new <name> adds another"
                        .to_string();
                if table.import_offer {
                    msg.push_str("\n/modelset import copies omp's model config");
                }
                return app.notice(&msg);
            }
            let rows = table
                .sets
                .into_iter()
                .map(|s| {
                    let mut detail = s.default.unwrap_or_else(|| "(no @default)".into());
                    if s.active {
                        detail.push_str(" · active");
                    }
                    (Item { label: s.name.clone(), detail }, s.name)
                })
                .collect();
            app.overlay =
                Some(Overlay::ModelSet { pick: Picker::from_rows(rows, |items| Select::new("Model sets", items)) });
        });
    }

    /// Makes `name` the active set and moves this session to its model.
    pub(super) fn switch_to(&mut self, name: &str) {
        let name = name.to_string();
        self.host_call(models_host::SWITCH_SET, &name.clone(), move |app, r: Result<Switched, String>| {
            let set = match r {
                Ok(s) => s,
                Err(e) => return app.error(&e),
            };
            if app.agent.status.plan && set.plan {
                app.send(Command::SetModel("@plan".into()));
            } else if let Some(v) = set.default {
                app.send(Command::SetModel("@default".into()));
                app.notice(&format!("model set {name}: @default = {v}"));
            } else {
                app.notice(&format!("model set {name} has no @default; this session keeps its model"));
            }
        });
    }

    pub(super) fn import_now(&mut self) {
        self.host_call(models_host::IMPORT_MODELS, &(), |app, r: Result<Imported, String>| match r {
            Ok(done) => {
                app.refresh_commands();
                app.notice(&format!(
                    "imported omp's model config into {}:\n{}",
                    done.path.display(),
                    done.lines.join("\n")
                ));
                if app.agent.status.model_id.is_empty() && done.has_default {
                    app.send(Command::SetModel("@default".into()));
                }
            }
            Err(e) => app.error(&e),
        });
    }

    /// Asks once at startup whether to copy omp's model config; declining asks again next start.
    pub(super) fn open_import_offer(&mut self, offer: &ImportOffer) {
        let lines = offer.lines.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n");
        let items = vec![
            Item { label: "Import".into(), detail: "copy it into models.toml".into() },
            Item { label: "Not now".into(), detail: "ask again next start; /modelset import copies it later".into() },
        ];
        let context = format!("mu now keeps model roles in {}. Found in omp:\n{lines}", offer.models_path.display());
        self.overlay =
            Some(Overlay::ModelImport { sel: Select::new("Import model config from omp?", items).context(&context) });
    }
}
