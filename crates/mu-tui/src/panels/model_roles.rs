use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use mu_base::models::{Thinking, split_level};
use mu_base::roles::RoleView;

use crate::widgets::select::{Item, Select, SelectResult};

#[derive(Debug, PartialEq, Eq)]
pub enum RoleEditorAction {
    Pending,
    Close,
    RequestModels,
    Save { set: String, role: String, value: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Roles,
    Edit,
    Models,
    Thinking,
}

pub struct RoleEditor {
    set: String,
    path: PathBuf,
    roles: Vec<RoleView>,
    role: Option<usize>,
    original: Option<String>,
    draft: Option<String>,
    screen: Screen,
    select: Select,
    models: Vec<String>,
    request: Option<u64>,
    message: String,
}

impl RoleEditor {
    pub fn new(set: String, path: PathBuf, roles: Vec<RoleView>, role: Option<&str>) -> Result<Self, String> {
        let selected = role
            .map(|name| roles.iter().position(|r| r.name == name).ok_or_else(|| format!("unknown model role @{name}")))
            .transpose()?;
        let mut editor = Self {
            set,
            path,
            roles,
            role: selected,
            original: None,
            draft: None,
            screen: Screen::Roles,
            select: Select::new("Model roles", vec![]),
            models: Vec::new(),
            request: None,
            message: String::new(),
        };
        if selected.is_some() {
            editor.open_edit();
        } else {
            editor.open_roles();
        }
        Ok(editor)
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
        self.select = Select::new("Model roles", items)
            .context(&format!("Saved configuration · set {}\n{}", self.set, self.path.display()))
            .details(details);
        if let Some(i) = self.role {
            let select = std::mem::replace(&mut self.select, Select::new("", vec![]));
            self.select = select.preselect(i);
        }
    }

    fn open_edit(&mut self) {
        let value = self.role.and_then(|i| self.roles[i].value.clone());
        self.original = value.clone();
        self.draft = value;
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
        let model_detail = if model.is_empty() {
            role.fallback.as_ref().map_or_else(
                || format!("Not assigned · {}", role.unset),
                |(r, v)| format!("Not assigned · fallback @{r} ({v})"),
            )
        } else {
            model
        };
        let fallback = role.fallback.as_ref().map_or_else(|| role.unset.clone(), |(r, v)| format!("@{r} ({v})"));
        let items = vec![
            Item { label: "Model".into(), detail: model_detail },
            Item { label: "Thinking".into(), detail: thinking },
            Item {
                label: "Clear assignment".into(),
                detail: format!("Save restores the existing fallback chain: {fallback}"),
            },
            Item {
                label: "Save".into(),
                detail: self.draft.clone().unwrap_or_else(|| format!("Clear; restore {fallback}")),
            },
            Item { label: "Cancel".into(), detail: "Discard changes and return to roles".into() },
        ];
        let mut context = format!("Saved configuration · set {}\n{}\n", self.set, self.path.display());
        context.push_str(&role.detail());
        if let Some(original) = &self.original {
            context.push_str(&format!("\nOriginal explicit value: {original}"));
        } else {
            context.push_str("\nOriginal explicit value: not assigned");
        }
        context.push_str(&format!("\nClearing restores the existing fallback chain: {fallback}"));
        if !self.message.is_empty() {
            context.push_str(&format!("\n{}", self.message));
        }
        self.select = Select::new(&format!("Edit @{}", role.name), items).context(&context).back_keys(true);
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
        self.role = name.and_then(|name| self.roles.iter().position(|r| r.name == name));
        self.original = None;
        self.draft = None;
        self.open_roles();
    }

    pub fn on_key(&mut self, key: KeyEvent) -> RoleEditorAction {
        if key.kind == KeyEventKind::Release {
            return RoleEditorAction::Pending;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return RoleEditorAction::Close;
        }
        match self.select.on_key(key) {
            SelectResult::Cancelled | SelectResult::Back => match self.screen {
                Screen::Roles => return RoleEditorAction::Close,
                Screen::Edit => {
                    self.draft = None;
                    self.open_roles();
                }
                Screen::Models | Screen::Thinking => self.show_edit(),
            },
            SelectResult::Chosen(indices) => {
                let Some(&i) = indices.first() else { return RoleEditorAction::Pending };
                match self.screen {
                    Screen::Roles if i < self.roles.len() => {
                        self.role = Some(i);
                        self.open_edit();
                    }
                    Screen::Edit => match i {
                        0 => return RoleEditorAction::RequestModels,
                        1 if self.draft.is_some() => self.show_thinking(),
                        1 => {
                            self.message = "Choose a model first".into();
                            self.show_edit();
                        }
                        2 => {
                            self.draft = None;
                            self.show_edit();
                        }
                        3 => {
                            if self.draft == self.original {
                                self.open_roles();
                            } else {
                                return RoleEditorAction::Save {
                                    set: self.set.clone(),
                                    role: self.roles[self.role.expect("edit requires a role")].name.clone(),
                                    value: self.draft.clone(),
                                };
                            }
                        }
                        4 => {
                            self.draft = None;
                            self.open_roles();
                        }
                        _ => {}
                    },
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
            _ => {}
        }
        RoleEditorAction::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn role(value: Option<&str>) -> RoleView {
        RoleView {
            name: "task".into(),
            purpose: "subagents".into(),
            used_by: "Agents".into(),
            value: value.map(str::to_string),
            fallback: Some(("default".into(), "a/fallback".into())),
            unset: "uses session's model".into(),
            agents: vec!["worker".into()],
        }
    }

    fn editor(value: Option<&str>) -> RoleEditor {
        RoleEditor::new("main".into(), "models.toml".into(), vec![role(value)], None).unwrap()
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
        assert_eq!(enter(&mut e), RoleEditorAction::Save { set: "main".into(), role: "task".into(), value: None });

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
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(e.on_key(ctrl_c), RoleEditorAction::Close);
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
