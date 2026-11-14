//! `/modelset`: switch, create, delete and rename model sets, and copy omp's model config into
//! `models.toml` (also offered once at startup).

use super::*;
use crate::widgets::overlay::Overlay;
use crate::widgets::picker::Picker;
use crate::widgets::select::{Item, Select};
use mu_base::config::models_path;

const USAGE: &str = "usage: /modelset [name | new <name> | delete <name> | rename <old> <new> | import]";

impl App {
    pub(super) fn modelset_cmd(&mut self, arg: &str) {
        let words: Vec<&str> = arg.split_whitespace().collect();
        let result = match words.as_slice() {
            [] => return self.open_set_picker(),
            ["import"] => return self.import_now(),
            [word] if !matches!(*word, "new" | "delete" | "rename") => return self.switch_to(word),
            ["new", name] => self.services.new_set(name).map(|from| {
                format!(
                    "model set {name} created from {} and active; /model edits its roles",
                    from.as_deref().unwrap_or("nothing")
                )
            }),
            ["delete", name] => self.services.delete_set(name).map(|()| format!("model set {name} deleted")),
            ["rename", old, new] => {
                self.services.rename_set(old, new).map(|()| format!("model set {old} renamed to {new}"))
            }
            _ => return self.error(USAGE),
        };
        match result {
            Ok(msg) => {
                self.refresh_commands();
                self.notice(&msg);
            }
            Err(e) => self.error(&e),
        }
    }

    pub(super) fn open_set_picker(&mut self) {
        let cfg = &self.services.providers.cfg;
        let names = cfg.set_names();
        if names.is_empty() {
            let mut msg =
                "no model sets yet: roles you save with /model go into set main; /modelset new <name> adds another"
                    .to_string();
            if cfg.import_offer.borrow().is_some() {
                msg.push_str("\n/modelset import copies omp's model config");
            }
            return self.notice(&msg);
        }
        let active = cfg.active_set();
        let rows = {
            let models = cfg.models.borrow();
            names
                .into_iter()
                .map(|n| {
                    let mut detail = models
                        .sets
                        .get(&n)
                        .and_then(|r| r.get("default"))
                        .cloned()
                        .unwrap_or_else(|| "(no @default)".into());
                    if active.as_deref() == Some(n.as_str()) {
                        detail.push_str(" · active");
                    }
                    (Item { label: n.clone(), detail }, n)
                })
                .collect()
        };
        self.overlay =
            Some(Overlay::ModelSet { pick: Picker::from_rows(rows, |items| Select::new("Model sets", items)) });
    }

    /// Makes `name` the active set and moves this session to its model.
    pub(super) fn switch_to(&mut self, name: &str) {
        if let Err(e) = self.services.switch_set(name) {
            return self.error(&e);
        }
        let cfg = self.services.providers.cfg.clone();
        if self.agent.status.plan && cfg.role("plan").is_some() {
            self.send(Command::SetModel("@plan".into()));
        } else if let Some(v) = cfg.role("default") {
            self.send(Command::SetModel("@default".into()));
            self.notice(&format!("model set {name}: @default = {v}"));
        } else {
            self.notice(&format!("model set {name} has no @default; this session keeps its model"));
        }
    }

    pub(super) fn import_now(&mut self) {
        match self.services.import_models() {
            Ok(lines) => {
                self.refresh_commands();
                self.notice(&format!(
                    "imported omp's model config into {}:\n{}",
                    models_path().display(),
                    lines.join("\n")
                ));
                if self.agent.status.model_id.is_empty() && self.services.providers.cfg.role("default").is_some() {
                    self.send(Command::SetModel("@default".into()));
                }
            }
            Err(e) => self.error(&e),
        }
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
