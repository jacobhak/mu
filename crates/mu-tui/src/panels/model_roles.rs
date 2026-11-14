use std::path::PathBuf;

use crossterm::event::{KeyEvent, KeyEventKind};
use mu_base::model_config::{RoleMeta, valid_set_name};
use mu_base::models::{Thinking, split_level};
use mu_base::roles::{self, RoleView};

use crate::widgets::select::{Item, Select, SelectResult};

#[derive(Debug, PartialEq, Eq)]
pub enum RoleEditorAction {
    Pending,
    Close,
    RequestModels,
    /// `meta` is a custom role's `[roles.<name>]` settings; `None` for a role mu uses itself.
    Save {
        set: String,
        role: String,
        value: Option<String>,
        meta: Option<RoleMeta>,
    },
    Delete {
        set: String,
        role: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Roles,
    Edit,
    Models,
    Thinking,
    Fallbacks,
    Description,
    Confirm,
}

/// A row of the edit screen; the rows a role has depend on whether it is a custom one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditRow {
    Model,
    Thinking,
    Fallbacks,
    Description,
    ClearDescription,
    Delegate,
    Clear,
    Delete,
    Save,
    Cancel,
}

pub struct RoleEditor {
    set: String,
    path: PathBuf,
    roles: Vec<RoleView>,
    role: Option<usize>,
    /// The role at `role` is a new, unsaved view.
    added: bool,
    original: Option<String>,
    draft: Option<String>,
    /// A custom role's draft `[roles.<name>]` settings; never written until Save.
    meta: Option<RoleMeta>,
    /// `meta` as it was when the editor opened, so an unchanged save is a no-op.
    meta_original: Option<RoleMeta>,
    screen: Screen,
    select: Select,
    /// The edit screen's rows, in the order they are shown.
    rows: Vec<EditRow>,
    models: Vec<String>,
    /// The role named by each row of the fallback screen.
    choices: Vec<String>,
    request: Option<u64>,
    message: String,
}

impl RoleEditor {
    pub fn new(set: String, path: PathBuf, roles: Vec<RoleView>, role: Option<&str>) -> Result<Self, String> {
        let selected = role.and_then(|name| roles.iter().position(|r| r.name == name));
        let mut editor = Self {
            set,
            path,
            roles,
            role: selected,
            added: false,
            original: None,
            draft: None,
            meta: None,
            meta_original: None,
            screen: Screen::Roles,
            select: Select::new("Model roles", vec![]),
            rows: Vec::new(),
            models: Vec::new(),
            choices: Vec::new(),
            request: None,
            message: String::new(),
        };
        match role {
            Some(name) if selected.is_none() => editor.start_role(name)?,
            _ if selected.is_some() => editor.open_edit(),
            _ => editor.open_roles(),
        }
        Ok(editor)
    }

    /// Selects `name`, or starts an unsaved role of that name when nothing has it.
    fn start_role(&mut self, name: &str) -> Result<(), String> {
        let name = name.trim().trim_start_matches('@');
        if !valid_set_name(name) {
            return Err("role names are a letter, then letters, digits, - or _".into());
        }
        match self.roles.iter().position(|r| r.name == name) {
            Some(i) => self.role = Some(i),
            None => {
                self.roles.push(roles::unsaved(name));
                self.role = Some(self.roles.len() - 1);
                self.added = true;
            }
        }
        self.open_edit();
        Ok(())
    }

    /// Leaves Edit without saving; a role started in this session and never saved disappears.
    fn cancel_edit(&mut self) {
        self.draft = None;
        self.meta = None;
        self.meta_original = None;
        if self.added {
            self.roles.pop();
            self.role = None;
            self.added = false;
        }
        self.open_roles();
    }

    pub fn select(&self) -> &Select {
        &self.select
    }

    pub fn select_mut(&mut self) -> &mut Select {
        &mut self.select
    }

    fn open_roles(&mut self) {
        self.screen = Screen::Roles;
        let items = self.roles.iter().map(|r| Item { label: format!("@{}", r.name), detail: r.row() }).collect();
        let details = self.roles.iter().map(|r| Some(format!("{}\nEnter edits this role.", r.detail()))).collect();
        let mut context = format!("Saved configuration · set {}\n{}", self.set, self.path.display());
        if !self.message.is_empty() {
            context.push_str(&format!("\n{}", self.message));
        }
        self.select = Select::new("Model roles", items).context(&context).details(details).free_text("New role…");
        self.message.clear();
        if let Some(i) = self.role {
            let select = std::mem::replace(&mut self.select, Select::new("", vec![]));
            self.select = select.preselect(i);
        }
    }

    fn open_edit(&mut self) {
        let value = self.role.and_then(|i| self.roles[i].value.clone());
        self.original = value.clone();
        self.draft = value;
        let role = &self.roles[self.role.expect("edit requires a role")];
        self.meta = role.custom.then(|| role.meta());
        self.meta_original = self.meta.clone();
        self.message.clear();
        self.show_edit();
    }

    fn show_edit(&mut self) {
        self.screen = Screen::Edit;
        self.request = None;
        let role = &self.roles[self.role.expect("edit requires a role")];
        let (model, thinking) = self.draft.as_deref().map(split_level).unwrap_or_default();
        let thinking = thinking.map_or_else(
            || "No override".to_string(),
            |t| {
                if t == Thinking::Low && self.draft.as_deref().is_some_and(|v| v.ends_with(":minimal")) {
                    "low".into()
                } else {
                    t.as_str().into()
                }
            },
        );
        let effective = self.effective_fallback(role);
        let fallback = effective.as_ref().map_or_else(|| role.unset.clone(), |(r, v)| format!("@{r} ({v})"));
        let model_detail = if !model.is_empty() {
            model
        } else if effective.is_some() {
            format!("Not assigned · fallback {fallback}")
        } else {
            format!("Not assigned · {}", role.unset)
        };
        let mut rows: Vec<(EditRow, Item)> = vec![
            (EditRow::Model, Item { label: "Model".into(), detail: model_detail }),
            (EditRow::Thinking, Item { label: "Thinking".into(), detail: thinking }),
        ];
        if let Some(meta) = &self.meta {
            let chain = if meta.fallback.is_empty() {
                "None · nothing is tried before @task's chain".to_string()
            } else {
                meta.fallback.iter().map(|r| format!("@{r}")).collect::<Vec<_>>().join(" then ")
            };
            rows.push((EditRow::Fallbacks, Item { label: "Fallbacks".into(), detail: chain }));
            rows.push((
                EditRow::Description,
                Item {
                    label: "Description".into(),
                    detail: meta.description.as_deref().and_then(|d| d.lines().next()).unwrap_or("None").into(),
                },
            ));
            if meta.description.is_some() {
                rows.push((
                    EditRow::ClearDescription,
                    Item { label: "Clear description".into(), detail: "The model then sees only the name".into() },
                ));
            }
            let delegate = if !meta.delegate {
                "Off".to_string()
            } else if meta.description.as_deref().is_none_or(|d| d.trim().is_empty()) {
                "On, but there is no description: the model sees only the name".to_string()
            } else {
                "On: the model may pick this role for a subagent".to_string()
            };
            rows.push((EditRow::Delegate, Item { label: "Delegate".into(), detail: delegate }));
        }
        let assigned = self.draft.is_some() || self.original.is_some();
        if assigned {
            rows.push((
                EditRow::Clear,
                Item {
                    label: "Clear assignment".into(),
                    detail: format!("Save restores the existing fallback chain: {fallback}"),
                },
            ));
        }
        if role.custom {
            rows.push((
                EditRow::Delete,
                Item {
                    label: "Delete role…".into(),
                    detail: format!("Removes it from set {} and its [roles.{}] settings", self.set, role.name),
                },
            ));
        }
        rows.push((
            EditRow::Save,
            Item {
                label: "Save".into(),
                detail: match (&self.draft, assigned) {
                    (Some(v), _) => v.clone(),
                    (None, true) => format!("Clear; restore {fallback}"),
                    (None, false) => "Write the role's settings".into(),
                },
            },
        ));
        rows.push((
            EditRow::Cancel,
            Item { label: "Cancel".into(), detail: "Discard changes and return to roles".into() },
        ));
        let mut context = format!("Saved configuration · set {}\n{}\n", self.set, self.path.display());
        context.push_str(&format!("Used by: {}", role.used_by));
        if !role.agents.is_empty() {
            context.push_str(&format!("\nAgents: {}", role.agents.join(", ")));
        }
        // A custom role's chain can be changed here, so its state comes from the draft, not the view.
        context.push_str(&if self.meta.is_some() {
            match (&self.draft, effective.is_some()) {
                (Some(v), _) => format!("\nSet to {v}"),
                (None, true) => format!("\nNot set: uses {fallback}."),
                (None, false) => format!("\nNot set: {}.", role.unset),
            }
        } else {
            role.state()
        });
        if let Some(original) = &self.original {
            context.push_str(&format!("\nOriginal explicit value: {original}"));
        } else {
            context.push_str("\nOriginal explicit value: not assigned");
        }
        if assigned {
            context.push_str(&format!("\nClearing restores the existing fallback chain: {fallback}"));
        }
        if !self.message.is_empty() {
            context.push_str(&format!("\n{}", self.message));
        }
        self.rows = rows.iter().map(|(row, _)| *row).collect();
        let items = rows.into_iter().map(|(_, item)| item).collect();
        self.select = Select::new(&format!("Edit @{}", role.name), items).context(&context).back_keys(true);
    }

    /// The role this one runs on when it has no model of its own: the first set role in its chain.
    /// A custom role's `fallback` list is read from the draft, so a fallback added here counts
    /// before it is saved, in the chain `roles::custom_chain` builds. A built-in role keeps the chain
    /// it was listed with.
    fn effective_fallback(&self, role: &RoleView) -> Option<(String, String)> {
        let Some(meta) = &self.meta else { return role.fallback.clone() };
        let set = |name: &str| self.roles.iter().find(|r| r.name == name).and_then(|r| r.value.clone());
        roles::first_set(&roles::custom_chain(&role.name, &meta.fallback), set)
    }

    /// Every role the edited one may fall back to, with its place in the chain.
    fn show_fallbacks(&mut self, keep: Option<&str>) {
        self.screen = Screen::Fallbacks;
        let role = &self.roles[self.role.expect("fallbacks require a role")];
        let (name, list) = (role.name.clone(), self.meta.as_ref().map(|m| m.fallback.clone()).unwrap_or_default());
        let choices: Vec<String> = self.roles.iter().map(|r| r.name.clone()).filter(|r| *r != name).collect();
        let mut items: Vec<Item> = choices
            .iter()
            .map(|r| {
                let at = list.iter().position(|f| f == r);
                let detail = match at {
                    Some(i) => format!("Fallback {} · Enter removes it", i + 1),
                    None => "Enter adds it at the end".into(),
                };
                Item { label: format!("@{r}"), detail }
            })
            .collect();
        items.push(Item { label: "Done".into(), detail: "Back to the role".into() });
        let chain = if list.is_empty() {
            "No fallbacks of its own".to_string()
        } else {
            format!("Tried in order: {}", list.iter().map(|f| format!("@{f}")).collect::<Vec<_>>().join(" then "))
        };
        let context = format!(
            "Fallbacks for @{name} · set {}\n{chain}\nThen @task's chain: agents naming @{name} use it when nothing here is set.",
            self.set
        );
        let highlighted = keep
            .and_then(|k| choices.iter().position(|c| c == k))
            .or_else(|| list.first().and_then(|f| choices.iter().position(|c| c == f)));
        let mut select = Select::new(&format!("Fallbacks for @{name}"), items).context(&context).back_keys(true);
        if let Some(i) = highlighted {
            select = select.preselect(i);
        }
        self.choices = choices;
        self.select = select;
    }

    fn show_description(&mut self) {
        self.screen = Screen::Description;
        let role = &self.roles[self.role.expect("description requires a role")];
        let current = self.meta.as_ref().and_then(|m| m.description.clone()).unwrap_or_default();
        let context = format!(
            "Description for @{}\nThe model reads it where it may pick this role for a subagent; the first line is shown beside the role.",
            role.name
        );
        self.select = Select::new(&format!("Describe @{}", role.name), vec![])
            .context(&context)
            .free_text("Description")
            .edit_text(&current)
            .back_keys(true);
    }

    fn show_confirm(&mut self) {
        self.screen = Screen::Confirm;
        let role = &self.roles[self.role.expect("delete requires a role")];
        let mut context = format!(
            "Delete @{name}?\nRemoves it from set {set} and its [roles.{name}] settings.",
            name = role.name,
            set = self.set
        );
        if !role.other_sets.is_empty() {
            context.push_str(&format!("\nIt stays set in {}.", role.other_sets.join(", ")));
        }
        if !role.agents.is_empty() {
            context.push_str(&format!("\nAgents naming it: {}.", role.agents.join(", ")));
        }
        context.push_str("\nThis cannot be undone: models.toml is written at once.");
        let items = vec![
            Item { label: format!("Delete @{}", role.name), detail: "models.toml loses the role".into() },
            Item { label: "Cancel".into(), detail: "Keep the role".into() },
        ];
        self.select = Select::new(&format!("Delete @{}?", role.name), items).context(&context).back_keys(true);
    }

    pub fn begin_models(&mut self, request: u64) {
        self.screen = Screen::Models;
        self.request = Some(request);
        self.models.clear();
        self.select = Select::new("Choose a model", vec![]).context("Loading models…").back_keys(true);
    }

    /// Fills the list for `request`. Each source's answer may call this again: the list is
    /// rebuilt and the highlighted model kept. Once the user leaves the list, chooses a model or
    /// clears the draft, the request no longer matches and nothing changes. `note` replaces the
    /// context line (a `providers.toml` that does not read).
    pub fn models_loaded(&mut self, request: u64, items: Vec<Item>, ids: Vec<String>, note: Option<&str>) -> bool {
        if self.screen != Screen::Models || self.request != Some(request) || items.len() != ids.len() {
            return false;
        }
        let highlighted = self.select.highlighted().and_then(|i| self.models.get(i)).cloned();
        let current = self.draft.as_deref().map(split_level).map(|(m, _)| m).filter(|m| !m.is_empty());
        let mut items = items;
        let mut ids = ids;
        if let Some(id) = current.as_ref().filter(|id| !ids.contains(id)) {
            items.push(Item { label: id.clone(), detail: "Current configured model".into() });
            ids.push(id.clone());
        }
        let selected =
            highlighted.filter(|h| ids.contains(h)).or(current).and_then(|id| ids.iter().position(|m| m == &id));
        let context = note.unwrap_or(if items.is_empty() {
            "No models available; use /login to configure a provider"
        } else {
            "Concrete models only"
        });
        self.select = Select::new("Choose a model", items).context(context).back_keys(true);
        if let Some(i) = selected {
            let select = std::mem::replace(&mut self.select, Select::new("", vec![]));
            self.select = select.preselect(i);
        }
        self.models = ids;
        true
    }

    fn show_thinking(&mut self) {
        self.screen = Screen::Thinking;
        let level = self.draft.as_deref().and_then(|v| split_level(v).1);
        let items = std::iter::once("No override")
            .chain(Thinking::NAMES)
            .map(|s| Item {
                label: s.into(),
                detail: if s == "No override" { "Do not force a thinking level".into() } else { String::new() },
            })
            .collect();
        let selected = level.and_then(|t| Thinking::NAMES.iter().position(|s| *s == t.as_str())).map_or(0, |i| i + 1);
        self.select = Select::new("Choose thinking level", items)
            .context("Saved configuration draft")
            .back_keys(true)
            .preselect(selected);
    }

    pub fn saved(&mut self, set: String, roles: Vec<RoleView>) {
        let name = self.role.map(|i| self.roles[i].name.clone());
        self.set = set;
        self.roles = roles;
        self.added = false;
        self.role = name.and_then(|name| self.roles.iter().position(|r| r.name == name));
        self.original = None;
        self.draft = None;
        self.meta = None;
        self.meta_original = None;
        self.open_roles();
    }

    pub fn on_key(&mut self, key: KeyEvent) -> RoleEditorAction {
        if key.kind == KeyEventKind::Release {
            return RoleEditorAction::Pending;
        }
        match self.select.on_key(key) {
            SelectResult::Cancelled | SelectResult::Back => match self.screen {
                Screen::Roles => return RoleEditorAction::Close,
                Screen::Edit => self.cancel_edit(),
                Screen::Models | Screen::Thinking | Screen::Fallbacks | Screen::Description | Screen::Confirm => {
                    self.show_edit()
                }
            },
            SelectResult::Chosen(indices) => {
                let Some(&i) = indices.first() else { return RoleEditorAction::Pending };
                match self.screen {
                    Screen::Roles if i < self.roles.len() => {
                        self.role = Some(i);
                        self.open_edit();
                    }
                    Screen::Edit => {
                        let Some(row) = self.rows.get(i).copied() else { return RoleEditorAction::Pending };
                        match row {
                            EditRow::Model => return RoleEditorAction::RequestModels,
                            EditRow::Thinking if self.draft.is_some() => self.show_thinking(),
                            EditRow::Thinking => {
                                self.message = "Choose a model first".into();
                                self.show_edit();
                            }
                            EditRow::Fallbacks => self.show_fallbacks(None),
                            EditRow::Description => self.show_description(),
                            EditRow::ClearDescription => {
                                if let Some(meta) = &mut self.meta {
                                    meta.description = None;
                                }
                                self.show_edit();
                            }
                            EditRow::Delegate => {
                                if let Some(meta) = &mut self.meta {
                                    meta.delegate = !meta.delegate;
                                }
                                self.show_edit();
                            }
                            EditRow::Clear => {
                                self.draft = None;
                                self.show_edit();
                            }
                            EditRow::Delete => self.show_confirm(),
                            EditRow::Save => {
                                if self.draft == self.original && self.meta == self.meta_original {
                                    self.cancel_edit();
                                } else {
                                    return RoleEditorAction::Save {
                                        set: self.set.clone(),
                                        role: self.roles[self.role.expect("edit requires a role")].name.clone(),
                                        value: self.draft.clone(),
                                        meta: self.meta.clone(),
                                    };
                                }
                            }
                            EditRow::Cancel => self.cancel_edit(),
                        }
                    }
                    Screen::Fallbacks if i < self.choices.len() => {
                        let choice = self.choices[i].clone();
                        if let Some(meta) = &mut self.meta {
                            match meta.fallback.iter().position(|f| *f == choice) {
                                Some(pos) => {
                                    meta.fallback.remove(pos);
                                }
                                None => meta.fallback.push(choice.clone()),
                            }
                        }
                        self.show_fallbacks(Some(&choice));
                    }
                    Screen::Fallbacks => self.show_edit(),
                    Screen::Confirm if i == 0 => {
                        return RoleEditorAction::Delete {
                            set: self.set.clone(),
                            role: self.roles[self.role.expect("delete requires a role")].name.clone(),
                        };
                    }
                    Screen::Confirm => self.show_edit(),
                    Screen::Models if i < self.models.len() => {
                        let id = &self.models[i];
                        let suffix = self
                            .draft
                            .as_deref()
                            .map(|v| {
                                let (model, _) = split_level(v);
                                &v[model.len()..]
                            })
                            .unwrap_or("");
                        self.draft = Some(format!("{id}{suffix}"));
                        self.show_edit();
                    }
                    Screen::Thinking if i <= Thinking::NAMES.len() => {
                        if let Some(value) = &self.draft {
                            let (model, _) = split_level(value);
                            self.draft =
                                Some(if i == 0 { model } else { format!("{}:{}", model, Thinking::NAMES[i - 1]) });
                        }
                        self.show_edit();
                    }
                    _ => {}
                }
            }
            SelectResult::Text(name) if self.screen == Screen::Roles => {
                if let Err(e) = self.start_role(&name) {
                    self.message = e;
                    self.open_roles();
                }
            }
            SelectResult::Text(text) if self.screen == Screen::Description => {
                if let Some(meta) = &mut self.meta {
                    let text = text.trim().to_string();
                    meta.description = (!text.is_empty()).then_some(text);
                }
                self.show_edit();
            }
            _ => {}
        }
        RoleEditorAction::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn role(value: Option<&str>) -> RoleView {
        RoleView {
            name: "task".into(),
            purpose: "subagents".into(),
            used_by: "Agents".into(),
            value: value.map(str::to_string),
            fallback: Some(("default".into(), "a/fallback".into())),
            unset: "uses session's model".into(),
            agents: vec!["worker".into()],
            description: None,
            fallbacks: Vec::new(),
            delegate: false,
            custom: false,
            other_sets: Vec::new(),
        }
    }

    /// A custom role, as `/model` lists one that a set assigns but nothing refers to.
    fn custom(name: &str, value: Option<&str>) -> RoleView {
        RoleView {
            name: name.into(),
            purpose: "custom role".into(),
            used_by: "nothing refers to it yet".into(),
            value: value.map(str::to_string),
            fallback: Some(("default".into(), "a/fallback".into())),
            unset: "agents use @task".into(),
            agents: Vec::new(),
            description: None,
            fallbacks: Vec::new(),
            delegate: false,
            custom: true,
            other_sets: Vec::new(),
        }
    }

    fn editor(value: Option<&str>) -> RoleEditor {
        RoleEditor::new("main".into(), "models.toml".into(), vec![role(value)], None).unwrap()
    }

    /// An editor on a custom @writer with the built-in @task and another custom role beside it.
    fn custom_editor(value: Option<&str>) -> RoleEditor {
        let roles = vec![role(None), custom("writer", value), custom("reviewer", None)];
        RoleEditor::new("main".into(), "models.toml".into(), roles, Some("writer")).unwrap()
    }

    fn meta(e: &RoleEditor) -> RoleMeta {
        e.meta.clone().expect("a custom role has settings")
    }

    fn press(e: &mut RoleEditor, code: KeyCode) -> RoleEditorAction {
        e.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn down(e: &mut RoleEditor, n: usize) {
        for _ in 0..n {
            press(e, KeyCode::Down);
        }
    }

    fn enter(e: &mut RoleEditor) -> RoleEditorAction {
        press(e, KeyCode::Enter)
    }

    fn item(id: &str) -> Item {
        Item { label: id.into(), detail: String::new() }
    }

    fn texts(e: &RoleEditor) -> Vec<String> {
        let (lines, _) = e.select().render(80, 40);
        lines.iter().map(|l| crate::text::line::line_text(l).trim_end().to_string()).collect()
    }

    /// Types `name` into the "New role…" row and submits it.
    fn new_role(e: &mut RoleEditor, name: &str) {
        down(e, 1);
        enter(e);
        assert!(e.select().is_editing(), "the free-text row is live");
        for c in name.chars() {
            press(e, KeyCode::Char(c));
        }
        enter(e);
    }

    #[test]
    fn role_shortcut_opens_editor_and_cancel_discards_draft() {
        let mut e =
            RoleEditor::new("main".into(), "models.toml".into(), vec![role(Some("a/old:high"))], Some("task")).unwrap();
        assert_eq!(e.screen, Screen::Edit);
        assert_eq!(e.select().highlighted(), Some(0));
        press(&mut e, KeyCode::Esc);
        assert_eq!(e.screen, Screen::Roles);
    }

    #[test]
    fn child_back_preserves_draft_and_model_only_change_keeps_raw_suffix() {
        for suffix in [":auto", ":inherit", ":minimal", ":high", ":odd"] {
            let mut e = editor(Some(&format!("a/old{suffix}")));
            enter(&mut e);
            assert_eq!(enter(&mut e), RoleEditorAction::RequestModels);
            e.begin_models(7);
            assert!(!e.models_loaded(6, vec![item("a/new")], vec!["a/new".into()], None));
            assert!(e.models_loaded(7, vec![item("a/new")], vec!["a/new".into()], None));
            press(&mut e, KeyCode::Down);
            enter(&mut e);
            let expected = if suffix == ":odd" { "a/new".to_string() } else { format!("a/new{suffix}") };
            assert_eq!(e.draft.as_deref(), Some(expected.as_str()));
        }
    }

    #[test]
    fn clear_thinking_save_and_noop_are_draft_only() {
        let mut e = editor(Some("a/model:high"));
        enter(&mut e);
        down(&mut e, 1);
        enter(&mut e);
        press(&mut e, KeyCode::Up);
        press(&mut e, KeyCode::Up);
        enter(&mut e);
        assert_eq!(e.draft.as_deref(), Some("a/model:low"));
        down(&mut e, 2);
        enter(&mut e);
        assert_eq!(e.draft, None);
        down(&mut e, 3);
        assert_eq!(
            enter(&mut e),
            RoleEditorAction::Save { set: "main".into(), role: "task".into(), value: None, meta: None }
        );

        let mut e = editor(Some("a/model:inherit"));
        enter(&mut e);
        down(&mut e, 3);
        enter(&mut e);
        assert_eq!(e.screen, Screen::Roles, "unchanged save is a no-op");
    }

    #[test]
    fn empty_model_list_and_missing_model_thinking_are_actionable() {
        let mut e = editor(None);
        enter(&mut e);
        down(&mut e, 1);
        enter(&mut e);
        assert_eq!(e.screen, Screen::Edit);
        assert_eq!(enter(&mut e), RoleEditorAction::RequestModels);
        e.begin_models(1);
        assert!(e.models_loaded(1, vec![], vec![], None));
        assert_eq!(e.screen, Screen::Models);
        press(&mut e, KeyCode::Esc);
        assert_eq!(e.screen, Screen::Edit);
        press(&mut e, KeyCode::Esc);
        assert_eq!(e.screen, Screen::Roles, "Esc steps back a screen");
        assert_eq!(press(&mut e, KeyCode::Esc), RoleEditorAction::Close);
    }

    #[test]
    fn a_new_role_is_typed_into_the_free_text_row_and_escaped_away() {
        let mut e = editor(None);
        new_role(&mut e, "writer");
        assert_eq!(e.screen, Screen::Edit);
        assert_eq!(e.roles[e.role.unwrap()].name, "writer");
        press(&mut e, KeyCode::Esc);
        assert_eq!(e.screen, Screen::Roles);
        assert_eq!(e.roles.len(), 1, "@writer was never saved");
        assert_eq!(e.select().highlighted(), Some(0));
    }

    #[test]
    fn an_invalid_new_role_name_is_refused_on_the_roles_screen() {
        let mut e = editor(None);
        new_role(&mut e, "1x");
        assert_eq!(e.screen, Screen::Roles);
        assert_eq!(e.roles.len(), 1);
        assert!(texts(&e).iter().any(|l| l.contains("role names are a letter")), "{:?}", texts(&e));
        match RoleEditor::new("main".into(), "models.toml".into(), vec![role(None)], Some("1x")) {
            Err(e) => assert!(e.contains("role names are a letter"), "{e}"),
            Ok(_) => panic!("an invalid name must be refused"),
        }
    }

    #[test]
    fn a_new_role_saves_its_chosen_model() {
        let mut e = editor(None);
        new_role(&mut e, "writer");
        assert_eq!(enter(&mut e), RoleEditorAction::RequestModels);
        e.begin_models(9);
        assert!(e.models_loaded(9, vec![item("a/b")], vec!["a/b".into()], None));
        enter(&mut e);
        assert_eq!(e.draft.as_deref(), Some("a/b"));
        down(&mut e, 7);
        assert_eq!(
            enter(&mut e),
            RoleEditorAction::Save {
                set: "main".into(),
                role: "writer".into(),
                value: Some("a/b".into()),
                meta: Some(RoleMeta::default()),
            }
        );
    }

    #[test]
    fn fallbacks_are_added_in_order_and_removed_where_they_are() {
        let mut e = custom_editor(None);
        down(&mut e, 2);
        enter(&mut e);
        assert_eq!(e.screen, Screen::Fallbacks);
        let shown = |e: &RoleEditor| texts(e).join("\n");
        assert!(shown(&e).contains("@task  Enter adds it at the end"), "{}", shown(&e));
        assert!(shown(&e).contains("No fallbacks of its own") && !shown(&e).contains("nothing is tried"));

        enter(&mut e);
        assert_eq!(meta(&e).fallback, ["task"]);
        assert!(shown(&e).contains("@task  Fallback 1 · Enter removes it"), "{}", shown(&e));

        down(&mut e, 1);
        enter(&mut e);
        assert_eq!(meta(&e).fallback, ["task", "reviewer"]);
        assert!(shown(&e).contains("Tried in order: @task then @reviewer"), "{}", shown(&e));

        press(&mut e, KeyCode::Up);
        enter(&mut e);
        assert_eq!(meta(&e).fallback, ["reviewer"], "removing one keeps the others' order");

        down(&mut e, 2);
        enter(&mut e);
        assert_eq!(e.screen, Screen::Edit, "Done goes back to the role");
        assert_eq!(meta(&e).fallback, ["reviewer"]);
        down(&mut e, 6);
        assert_eq!(
            enter(&mut e),
            RoleEditorAction::Save {
                set: "main".into(),
                role: "writer".into(),
                value: None,
                meta: Some(RoleMeta { fallback: vec!["reviewer".into()], ..RoleMeta::default() }),
            }
        );
    }

    #[test]
    fn a_role_without_a_model_shows_the_chain_it_drafts() {
        // @writer as a role started in this session reads: no model, and nothing resolved yet.
        let mut writer = custom("writer", None);
        writer.fallback = None;
        let mut smol = custom("smol", Some("s/m"));
        smol.custom = false;
        smol.fallback = None;
        let roles = vec![role(Some("t/m")), writer, smol];
        let mut e = RoleEditor::new("main".into(), "models.toml".into(), roles, Some("writer")).unwrap();
        let shown = |e: &RoleEditor| texts(e).join("\n");
        assert!(shown(&e).contains("Not assigned · fallback @task (t/m)"), "{}", shown(&e));
        assert!(shown(&e).contains("Save  Write the role's settings"), "{}", shown(&e));
        assert!(
            !shown(&e).contains("Clear assignment"),
            "nothing is assigned, so nothing can be cleared: {}",
            shown(&e)
        );

        down(&mut e, 2);
        enter(&mut e);
        down(&mut e, 1);
        enter(&mut e);
        assert_eq!(meta(&e).fallback, ["smol"]);
        press(&mut e, KeyCode::Esc);
        assert_eq!(e.screen, Screen::Edit);
        let after = shown(&e);
        assert!(after.contains("Not assigned · fallback @smol (s/m)"), "a fallback set here counts at once: {after}");
        assert!(!after.contains("agents use @task"), "{after}");
        assert!(!after.contains("Clear"), "neither the clear row nor a clear on save: {after}");
    }

    #[test]
    fn a_description_and_delegation_are_edited_and_cleared() {
        let mut e = custom_editor(None);
        down(&mut e, 3);
        enter(&mut e);
        assert_eq!(e.screen, Screen::Description);
        assert!(e.select().is_editing());
        for c in "Long-form docs".chars() {
            press(&mut e, KeyCode::Char(c));
        }
        enter(&mut e);
        assert_eq!(e.screen, Screen::Edit);
        assert_eq!(meta(&e).description.as_deref(), Some("Long-form docs"));
        let shown = |e: &RoleEditor| texts(e).join("\n");
        assert!(shown(&e).contains("Description  Long-form docs"), "{}", shown(&e));

        down(&mut e, 5);
        assert!(shown(&e).contains("Delegate  Off"), "{}", shown(&e));
        enter(&mut e);
        assert!(meta(&e).delegate);
        assert!(shown(&e).contains("Delegate  On: the model may pick this role for a subagent"), "{}", shown(&e));
        down(&mut e, 4);
        enter(&mut e);
        assert_eq!(meta(&e).description, None, "Clear description drops it");
        assert!(!shown(&e).contains("Clear description"), "{}", shown(&e));
    }

    #[test]
    fn deleting_a_custom_role_needs_a_confirmation_naming_its_other_uses() {
        let mut e = custom_editor(None);
        e.roles[1].other_sets = vec!["work".into()];
        e.roles[1].agents = vec!["docs".into()];
        e.open_edit();
        down(&mut e, 5);
        enter(&mut e);
        assert_eq!(e.screen, Screen::Confirm);
        let shown = texts(&e).join("\n");
        assert!(shown.contains("stays set in work") && shown.contains("Agents naming it: docs"), "{shown}");

        down(&mut e, 1);
        enter(&mut e);
        assert_eq!(e.screen, Screen::Edit, "Cancel keeps the role");
        down(&mut e, 5);
        enter(&mut e);
        assert_eq!(enter(&mut e), RoleEditorAction::Delete { set: "main".into(), role: "writer".into() });

        let mut e = editor(None);
        enter(&mut e);
        assert!(texts(&e).iter().all(|l| !l.contains("Delete")), "{:?}", texts(&e));
    }

    #[test]
    fn an_unchanged_custom_role_is_not_saved() {
        let mut w = custom("writer", Some("a/m"));
        w.description = Some("Docs".into());
        w.fallbacks = vec!["smol".into()];
        let mut e = RoleEditor::new("main".into(), "models.toml".into(), vec![role(None), w], Some("writer")).unwrap();
        // Model, Thinking, Fallbacks, Description, Clear description, Delegate, Clear, Delete, Save, Cancel
        down(&mut e, 8);
        enter(&mut e);
        assert_eq!(e.screen, Screen::Roles, "nothing changed, so Save is a no-op");
        assert!(e.meta.is_none());
    }

    #[test]
    fn repeated_updates_keep_the_highlight_and_stop_once_the_user_moves_on() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        let rows = |v: &[&str]| v.iter().map(|s| item(s)).collect::<Vec<Item>>();
        let mut e = editor(None);
        enter(&mut e);
        assert_eq!(enter(&mut e), RoleEditorAction::RequestModels);
        e.begin_models(3);
        assert!(e.models_loaded(3, rows(&["a/a", "a/b"]), ids(&["a/a", "a/b"]), None));
        down(&mut e, 1);
        assert!(e.models_loaded(3, rows(&["a/a", "a/b", "a/c"]), ids(&["a/a", "a/b", "a/c"]), None));
        assert_eq!(e.select().highlighted(), Some(1), "the refresh keeps the highlighted model");
        assert!(e.models_loaded(3, rows(&["a/a", "a/b"]), ids(&["a/a", "a/b"]), Some("providers.toml is bad")));
        assert_eq!(e.select().highlighted(), Some(1));
        assert!(!e.models_loaded(2, rows(&["a/x"]), ids(&["a/x"]), None), "another request is not this list");

        press(&mut e, KeyCode::Esc);
        assert!(!e.models_loaded(3, rows(&["a/z"]), ids(&["a/z"]), None), "after leaving, the list is not revived");

        let mut e = editor(None);
        enter(&mut e);
        enter(&mut e);
        e.begin_models(4);
        assert!(e.models_loaded(4, rows(&["a/a"]), ids(&["a/a"]), None));
        enter(&mut e);
        assert!(!e.models_loaded(4, rows(&["a/a"]), ids(&["a/a"]), None), "a chosen model ends the list");
        assert_eq!(e.draft.as_deref(), Some("a/a"));
    }
}
